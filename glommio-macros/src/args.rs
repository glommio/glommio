//! Attribute argument parsing, shared by `#[main]` and `#[test]`.
//!
//! Three keys, deliberately: `placement`, `name` and `crate`. Anything the
//! builder does better stays on the builder.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    Ident, LitStr, Path, Token,
};

pub(crate) struct Args {
    /// Fully qualified, e.g. `::glommio::Placement::Fixed(0)`.
    pub(crate) placement: TokenStream,
    pub(crate) name: Option<LitStr>,
    pub(crate) krate: Path,
}

/// The `Placement` variants a caller may name without qualifying them.
///
/// Anything else given to `placement` is emitted as written, so a call, a
/// path through another module, or a local binding all work. The shorthand is
/// only a shorthand.
const BARE_VARIANTS: &[&str] = &["Unbound", "Fenced", "Fixed"];

/// Whether `expr` names a [`BARE_VARIANTS`] variant, bare, and so wants
/// `Placement::` in front of it.
fn is_bare_variant(expr: &syn::Expr) -> bool {
    let path = match expr {
        syn::Expr::Path(path) => &path.path,
        syn::Expr::Call(call) => match &*call.func {
            syn::Expr::Path(path) => &path.path,
            _ => return false,
        },
        _ => return false,
    };
    path.leading_colon.is_none()
        && path.segments.len() == 1
        && BARE_VARIANTS
            .iter()
            .any(|variant| path.segments[0].ident == variant)
}

enum Arg {
    Placement(syn::Expr),
    Name(LitStr),
    Crate(Path),
}

impl Parse for Arg {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        // `crate` is a keyword, so it does not parse as an Ident.
        if input.peek(Token![crate]) {
            input.parse::<Token![crate]>()?;
            input.parse::<Token![=]>()?;
            return Ok(Arg::Crate(input.parse()?));
        }

        let key: Ident = input.parse()?;
        input.parse::<Token![=]>()?;

        match key.to_string().as_str() {
            "placement" => Ok(Arg::Placement(input.parse()?)),
            "name" => Ok(Arg::Name(input.parse()?)),
            other => Err(syn::Error::new_spanned(
                &key,
                format!("unknown argument `{other}`; expected `placement` or `crate`"),
            )),
        }
    }
}

/// Parses the attribute arguments, defaulting the placement to
/// `default_placement` when the caller did not name one.
pub(crate) fn parse(args: proc_macro::TokenStream, default_placement: &str) -> syn::Result<Args> {
    let parsed = syn::parse::Parser::parse(Punctuated::<Arg, Token![,]>::parse_terminated, args)?;

    let mut placement = None;
    let mut name = None;
    let mut krate = None;

    for arg in parsed {
        match arg {
            Arg::Placement(value) => placement = Some(value),
            Arg::Name(value) => name = Some(value),
            Arg::Crate(value) => krate = Some(value),
        }
    }

    let krate = krate.unwrap_or_else(|| syn::parse_quote!(::glommio));
    let placement = match placement {
        // A bare variant name is qualified for the caller; anything else is
        // theirs and goes through untouched, so a computed placement needs no
        // escape from the attribute.
        Some(expr) if is_bare_variant(&expr) => quote!(#krate::Placement::#expr),
        Some(expr) => quote!(#expr),
        None => {
            let default: Ident = Ident::new(default_placement, proc_macro2::Span::call_site());
            quote!(#krate::Placement::#default)
        }
    };

    Ok(Args {
        placement,
        name,
        krate,
    })
}
