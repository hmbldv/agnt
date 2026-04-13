//! # agnt-macros
//!
//! Proc-macros for the [agnt](https://crates.io/crates/agnt) agent runtime.
//!
//! ## `#[tool]` attribute
//!
//! Apply `#[agnt_macros::tool]` (or `#[agnt::tool]` when re-exported from the
//! flagship crate) to a free function to generate a unit struct plus a
//! [`TypedTool`](../agnt_core/tool/trait.TypedTool.html) impl.
//!
//! ```ignore
//! use agnt_macros::tool;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize)]
//! struct AddArgs { a: i64, b: i64 }
//! #[derive(Serialize)]
//! struct AddOut { sum: i64 }
//!
//! /// Add two integers and return their sum.
//! #[tool]
//! fn add(args: AddArgs) -> Result<AddOut, String> {
//!     Ok(AddOut { sum: args.a + args.b })
//! }
//!
//! // Generates:
//! //   pub struct Add;
//! //   impl agnt_core::TypedTool for Add { ... NAME = "add" ... }
//! ```
//!
//! ### Requirements on the annotated function
//!
//! * Exactly one argument whose type becomes `TypedTool::Args`.
//! * Return type must be `Result<Output, Error>`.
//! * A doc comment is strongly recommended — it becomes the tool description
//!   the model sees. If absent, the function name is used as a fallback.
//!
//! ### ⚠️ Known limitations (v0.3.x)
//!
//! **`schema()` is a placeholder — the model sees no field information.**
//! The generated `TypedTool::schema` returns the literal value
//! `{"type": "object"}` with no `properties`, no `required`, no field
//! types. Consequences:
//!
//! * The model cannot see what arguments your tool accepts, so it will
//!   guess field names from the description alone.
//! * A wrong guess produces a `serde_json` deserialization error that is
//!   surfaced as the tool result; the model then has to re-plan from the
//!   error message.
//! * For any non-trivial tool, the macro currently *reduces* ergonomics
//!   versus hand-writing a [`TypedTool`] impl where you control
//!   `schema()` and can emit a real JSON Schema.
//!
//! This will be fixed in v0.4 behind an opt-in `#[tool(schema = schemars)]`
//! attribute that wires the annotated `Args` type through `schemars` to
//! produce a real JSON Schema. Until then, prefer a hand-written
//! `TypedTool` impl for any tool whose arguments are non-obvious from the
//! description alone.
//!
//! ### Other limitations
//!
//! * Only free functions are supported; methods and closures are not.
//! * The function is left in place unchanged, so you can still call it
//!   directly. The generated struct's `TypedTool::call` simply forwards.
//!
//! [`TypedTool`]: ../agnt_core/tool/trait.TypedTool.html

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, spanned::Spanned, Attribute, Expr, ExprLit, FnArg, ItemFn, Lit, Meta,
    PatType, ReturnType, Type,
};

/// Generate a [`TypedTool`] impl from a free function.
///
/// ⚠️ **v0.3.x limitation**: the generated `schema()` returns a bare
/// `{"type": "object"}` with no field metadata. The model cannot see
/// your argument names or types from the schema alone and must infer
/// them from the description. See the [crate-level docs](crate) for
/// the full list of limitations and the v0.4 plan.
#[proc_macro_attribute]
pub fn tool(_args: TokenStream, input: TokenStream) -> TokenStream {
    let func = parse_macro_input!(input as ItemFn);
    match expand_tool(func) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_tool(func: ItemFn) -> syn::Result<TokenStream2> {
    let fn_name = func.sig.ident.clone();
    let fn_name_str = fn_name.to_string();
    let struct_name = format_ident!("{}", snake_to_pascal(&fn_name_str));

    // ---- argument type ----
    let inputs = &func.sig.inputs;
    if inputs.len() != 1 {
        return Err(syn::Error::new(
            func.sig.inputs.span(),
            format!(
                "#[tool] expects exactly one function argument (got {}); \
                 the argument type becomes TypedTool::Args",
                inputs.len()
            ),
        ));
    }
    let args_ty: &Type = match inputs.first().unwrap() {
        FnArg::Typed(PatType { ty, .. }) => ty.as_ref(),
        FnArg::Receiver(r) => {
            return Err(syn::Error::new(
                r.span(),
                "#[tool] cannot be applied to methods taking `self`",
            ));
        }
    };

    // ---- return type: Result<Output, Error> ----
    let (output_ty, error_ty) = match &func.sig.output {
        ReturnType::Default => {
            return Err(syn::Error::new(
                func.sig.output.span(),
                "#[tool] functions must return Result<Output, Error>",
            ));
        }
        ReturnType::Type(_, ty) => extract_result_types(ty)?,
    };

    // ---- doc comment / description ----
    let description = extract_doc(&func.attrs).unwrap_or_else(|| fn_name_str.clone());

    // Note: we intentionally do not emit a warning if description is missing;
    // stable proc-macros have no warning API. Fallback is silent-by-design.

    let vis = &func.vis;

    let expanded = quote! {
        #func

        #[allow(non_camel_case_types)]
        #vis struct #struct_name;

        impl ::agnt_core::TypedTool for #struct_name {
            type Args = #args_ty;
            type Output = #output_ty;
            type Error = #error_ty;
            const NAME: &'static str = #fn_name_str;
            const DESCRIPTION: &'static str = #description;

            fn schema() -> ::serde_json::Value {
                ::serde_json::json!({ "type": "object" })
            }

            fn call(&self, args: Self::Args) -> ::core::result::Result<Self::Output, Self::Error> {
                #fn_name(args)
            }
        }
    };

    Ok(expanded)
}

/// Walk `#[doc = "..."]` attributes, trim and join into a single description.
fn extract_doc(attrs: &[Attribute]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
            {
                parts.push(s.value().trim().to_string());
            }
        }
    }
    let joined = parts.join(" ").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Given a return type, verify it is `Result<O, E>` and return `(O, E)`.
fn extract_result_types(ty: &Type) -> syn::Result<(Type, Type)> {
    let err = || {
        syn::Error::new(
            ty.span(),
            "#[tool] functions must return Result<Output, Error> \
             (fully-qualified paths like std::result::Result are also accepted)",
        )
    };
    let path = match ty {
        Type::Path(tp) => &tp.path,
        _ => return Err(err()),
    };
    let seg = path.segments.last().ok_or_else(err)?;
    if seg.ident != "Result" {
        return Err(err());
    }
    let args = match &seg.arguments {
        syn::PathArguments::AngleBracketed(a) => &a.args,
        _ => return Err(err()),
    };
    let mut types = args.iter().filter_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    });
    let ok_ty = types.next().ok_or_else(err)?;
    let err_ty = types.next().ok_or_else(err)?;
    Ok((ok_ty, err_ty))
}

/// Convert `snake_case` (or already-PascalCase) to `PascalCase`.
fn snake_to_pascal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = true;
    for c in s.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        // Should be unreachable — syn would reject empty ident — but guard anyway.
        return "_Tool".to_string();
    }
    out
}

