//! Attribute macros for glommio.
//!
//! The expansion names the runtime crate by path, so everything here emits
//! `::glommio::…` unless the caller overrides it with `crate = <ident>`.

mod args;

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

/// Builds a `LocalExecutor` and runs an async `main` on it.
#[proc_macro_attribute]
pub fn main(args: TokenStream, item: TokenStream) -> TokenStream {
    expand(args, item, "Unbound", false)
}

/// Builds a `LocalExecutor` and runs an async test on it.
///
/// Emits a plain `#[test]` function, so `#[should_panic]`, `#[ignore]` and
/// `Result` returns compose without this macro knowing about them.
#[proc_macro_attribute]
pub fn test(args: TokenStream, item: TokenStream) -> TokenStream {
    expand(args, item, "Unbound", true)
}

fn expand(
    args: TokenStream,
    item: TokenStream,
    default_placement: &str,
    is_test: bool,
) -> TokenStream {
    let args = match args::parse(args, default_placement) {
        Ok(args) => args,
        Err(err) => return err.to_compile_error().into(),
    };

    let function = parse_macro_input!(item as ItemFn);

    if function.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            function.sig.fn_token,
            "expected an `async fn`; this attribute builds an executor to run one on",
        )
        .to_compile_error()
        .into();
    }

    if let Some(unsafety) = function.sig.unsafety {
        return syn::Error::new_spanned(
            unsafety,
            "expected a safe `async fn`; a `main`/`test` entry point cannot be \
             `unsafe` because the executor is what calls it, not the caller. \
             Move the unsafe code inside the body instead",
        )
        .to_compile_error()
        .into();
    }

    if let Some(constness) = function.sig.constness {
        return syn::Error::new_spanned(
            constness,
            "expected a non-`const` `async fn`; `const` and `async` cannot be \
             combined, so a `main`/`test` entry point cannot be `const` either",
        )
        .to_compile_error()
        .into();
    }

    if let Some(abi) = &function.sig.abi {
        return syn::Error::new_spanned(
            abi,
            "expected a plain `async fn`; an explicit ABI is not meaningful on a \
             `main`/`test` entry point, which the executor calls directly",
        )
        .to_compile_error()
        .into();
    }

    if !function.sig.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &function.sig.generics,
            "expected a non-generic `async fn`; `main`/`test` entry points cannot be \
             generic because nothing instantiates the parameters. Build the \
             executor with `LocalExecutorBuilder` inside a non-generic wrapper if \
             you need one",
        )
        .to_compile_error()
        .into();
    }

    if let Some(where_clause) = &function.sig.generics.where_clause {
        return syn::Error::new_spanned(
            where_clause,
            "expected a `where`-clause-free `async fn`; `main`/`test` entry points \
             cannot be generic because nothing instantiates the parameters. Build \
             the executor with `LocalExecutorBuilder` inside a non-generic wrapper \
             if you need one",
        )
        .to_compile_error()
        .into();
    }

    if !function.sig.inputs.is_empty() {
        return syn::Error::new_spanned(
            &function.sig.inputs,
            "expected a function taking no arguments; the executor has nothing to pass one",
        )
        .to_compile_error()
        .into();
    }

    let attrs = &function.attrs;
    let vis = &function.vis;
    let ident = &function.sig.ident;
    let output = &function.sig.output;
    let body = &function.block;

    let krate = &args.krate;
    let placement = &args.placement;
    // `name` is rejected rather than emitted: the expansion builds the executor
    // with `make()`, which runs it on the CALLING thread and never reads the
    // builder's name -- only `spawn()` does, where the name becomes the name of
    // the thread it creates. Threading the argument through anyway would accept
    // an option and silently drop it.
    if let Some(name) = &args.name {
        return syn::Error::new_spanned(
            name,
            "`name` cannot be honoured here: this attribute builds the executor on the calling \
             thread with `make()`, which has no thread of its own to name. Use \
             `LocalExecutorBuilder::new(placement).name(..).spawn(..)` if you want a named \
             executor thread",
        )
        .to_compile_error()
        .into();
    }
    let test_attr = if is_test {
        quote!(#[::core::prelude::v1::test])
    } else {
        quote!()
    };

    quote! {
        #test_attr
        #(#attrs)*
        #vis fn #ident() #output {
            #krate::LocalExecutorBuilder::new(#placement)
                .make()
                .expect("failed to create the glommio executor")
                .run(async move #body)
        }
    }
    .into()
}
