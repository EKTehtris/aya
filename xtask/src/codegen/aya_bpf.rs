use std::{fs::File, io::Write, path::PathBuf, process::Command};

use anyhow::anyhow;
use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use structopt::StructOpt;
use syn::{
    self, parse_str,
    punctuated::Punctuated,
    token::Comma,
    visit_mut::{self, VisitMut},
    AngleBracketedGenericArguments, ForeignItemStatic, GenericArgument, Ident, Item,
    PathArguments::AngleBracketed,
    Type,
};

#[derive(StructOpt)]
pub struct CodegenOptions {
    #[structopt(long)]
    libbpf_dir: PathBuf,
}

pub fn codegen(opts: CodegenOptions) -> Result<(), anyhow::Error> {
    let dir = PathBuf::from("bpf/aya-bpf");
    let generated = dir.join("src/bpf/generated");

    let types: Vec<&str> = vec!["bpf_map_.*"];
    let vars = vec!["BPF_.*", "bpf_.*"];
    let mut cmd = Command::new("bindgen");
    cmd.arg("--no-layout-tests")
        .arg("--use-core")
        .arg("--ctypes-prefix")
        .arg("::aya_bpf_cty")
        .arg("--default-enum-style")
        .arg("consts")
        .arg("--no-prepend-enum-name")
        .arg(&*dir.join("include/aya_bpf_bindings.h").to_string_lossy());

    for x in types {
        cmd.arg("--whitelist-type").arg(x);
    }

    for x in vars {
        cmd.arg("--whitelist-var").arg(x);
    }

    cmd.arg("--");
    cmd.arg("-I").arg(opts.libbpf_dir.join("src"));

    let output = cmd.output()?;
    let bindings = std::str::from_utf8(&output.stdout)?;

    if !output.status.success() {
        eprintln!("{}", std::str::from_utf8(&output.stderr)?);
        return Err(anyhow!("bindgen failed: {}", output.status));
    }

    // delete the helpers, then rewrite them in helpers.rs
    let mut tree = parse_str::<syn::File>(bindings).unwrap();
    let mut tx = RewriteBpfHelpers {
        helpers: Vec::new(),
    };
    tx.visit_file_mut(&mut tree);

    let filename = generated.join("bindings.rs");
    {
        let mut file = File::create(&filename)?;
        write!(file, "{}", tree.to_token_stream())?;
    }
    Command::new("rustfmt").arg(filename).status()?;

    let filename = generated.join("helpers.rs");
    {
        let mut file = File::create(&filename)?;
        write!(file, "use crate::bpf::generated::bindings::*;")?;
        for helper in &tx.helpers {
            file.write(helper.as_bytes())?;
        }
    }
    Command::new("rustfmt").arg(filename).status()?;

    Ok(())
}

struct RewriteBpfHelpers {
    helpers: Vec<String>,
}

impl VisitMut for RewriteBpfHelpers {
    fn visit_item_mut(&mut self, item: &mut Item) {
        visit_mut::visit_item_mut(self, item);
        if let Item::ForeignMod(_) = item {
            *item = Item::Verbatim(TokenStream::new())
        }
    }
    fn visit_foreign_item_static_mut(&mut self, static_item: &mut ForeignItemStatic) {
        if let Type::Path(path) = &*static_item.ty {
            let ident = &static_item.ident;
            let ident_str = ident.to_string();
            let last = path.path.segments.last().unwrap();
            let ty_ident = last.ident.to_string();
            if ident_str.starts_with("bpf_") && ty_ident == "Option" {
                let fn_ty = match &last.arguments {
                    AngleBracketed(AngleBracketedGenericArguments { args, .. }) => {
                        args.first().unwrap()
                    }
                    _ => panic!(),
                };
                let mut ty_s = quote! {
                    #[inline(always)]
                    pub #fn_ty
                }
                .to_string();
                ty_s = ty_s.replace("fn (", &format!("fn {} (", ident_str));
                let call_idx = self.helpers.len() + 1;
                let args: Punctuated<Ident, Comma> = match fn_ty {
                    GenericArgument::Type(Type::BareFn(f)) => f
                        .inputs
                        .iter()
                        .map(|arg| arg.name.clone().unwrap().0)
                        .collect(),
                    _ => unreachable!(),
                };
                let body = quote! {
                    {
                        let f: #fn_ty = ::core::mem::transmute(#call_idx);
                        f(#args)
                    }
                }
                .to_string();
                ty_s.push_str(&body);
                let mut helper = ty_s;
                if helper.contains("printk") {
                    helper = format!("/* {} */", helper);
                }
                self.helpers.push(helper);
            }
        }
    }
}