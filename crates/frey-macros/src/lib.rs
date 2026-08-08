//! Procedural macros for [Frey](https://github.com/newsbubbles/frey).
//!
//! The interesting design decision here is that **doc comments on parameters become schema
//! descriptions**. That is not a nicety: Anthropic's tool search matches on tool names,
//! descriptions, *argument names, and argument descriptions*, so an undocumented parameter is lost
//! search surface. Once a catalog outgrows the context window, an undocumented parameter makes a
//! tool measurably harder to find. Writing the doc comment is therefore the same act as making the
//! tool discoverable, which is the only way that habit survives contact with a deadline.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Attribute, Expr, ExprLit, FnArg, ItemFn, Lit, Meta, Pat, PatType, Result, Token, Type,
    parse_macro_input, punctuated::Punctuated, spanned::Spanned,
};

/// Turn an `async fn` into a Frey tool.
///
/// ```ignore
/// #[frey::tool(capabilities("fs:read"), cost_hint = "cheap")]
/// /// Read a file from the workspace and return its contents as text.
/// async fn fs_read(
///     /// Path relative to the workspace root.
///     path: String,
/// ) -> Result<String, ToolError> { … }
/// ```
///
/// The function's own doc comment becomes the tool description; each parameter's doc comment
/// becomes that property's `description` in the JSON Schema.
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let function = parse_macro_input!(item as ItemFn);
    match expand(&args, &function) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[derive(Debug)]
struct Options {
    description: Option<String>,
    capabilities: Vec<String>,
    cost_hint: Option<String>,
    caller: Option<String>,
    presentation: Option<String>,
}

fn parse_options(args: &Punctuated<Meta, Token![,]>) -> Result<Options> {
    let mut out = Options {
        description: None,
        capabilities: Vec::new(),
        cost_hint: None,
        caller: None,
        presentation: None,
    };

    for meta in args {
        match meta {
            Meta::NameValue(nv) => {
                let key = nv.path.get_ident().map(ToString::to_string).unwrap_or_default();
                let value = literal_string(&nv.value)
                    .ok_or_else(|| syn::Error::new(nv.value.span(), "expected a string literal"))?;
                match key.as_str() {
                    "description" => out.description = Some(value),
                    "cost_hint" => out.cost_hint = Some(value),
                    "caller" => out.caller = Some(value),
                    "presentation" => out.presentation = Some(value),
                    other => {
                        return Err(syn::Error::new(
                            nv.path.span(),
                            format!(
                                "unknown option `{other}`; expected one of description, \
                                 cost_hint, caller, presentation, or capabilities(..)"
                            ),
                        ));
                    }
                }
            }
            Meta::List(list) if list.path.is_ident("capabilities") => {
                let items =
                    list.parse_args_with(Punctuated::<Expr, Token![,]>::parse_terminated)?;
                for item in items {
                    let value = literal_string(&item).ok_or_else(|| {
                        syn::Error::new(item.span(), "capabilities take string literals")
                    })?;
                    out.capabilities.push(value);
                }
            }
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "expected `key = \"value\"` or `capabilities(\"..\")`",
                ));
            }
        }
    }
    Ok(out)
}

fn literal_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) => Some(s.value()),
        _ => None,
    }
}

/// Join a run of `#[doc = "..."]` attributes into one trimmed string.
fn doc_of(attrs: &[Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta
            && let Some(text) = literal_string(&nv.value)
        {
            lines.push(text.trim().to_string());
        }
    }
    let joined = lines.join(" ").trim().to_string();
    if joined.is_empty() { None } else { Some(joined) }
}

struct Param {
    ident: syn::Ident,
    ty: Type,
    doc: Option<String>,
}

fn expand(
    args: &Punctuated<Meta, Token![,]>,
    function: &ItemFn,
) -> Result<proc_macro2::TokenStream> {
    let options = parse_options(args)?;
    let fn_name = &function.sig.ident;
    let vis = &function.vis;

    let description = options.description.or_else(|| doc_of(&function.attrs)).ok_or_else(|| {
        syn::Error::new(
            function.sig.ident.span(),
            "a tool needs a description: add a doc comment or `description = \"...\"`. \
                 Tool search matches on it, so a tool without one is hard to find once the \
                 catalog outgrows the context window.",
        )
    })?;

    let mut params = Vec::new();
    for input in &function.sig.inputs {
        let FnArg::Typed(PatType { attrs, pat, ty, .. }) = input else {
            return Err(syn::Error::new(input.span(), "tools are free functions, not methods"));
        };
        let Pat::Ident(ident) = &**pat else {
            return Err(syn::Error::new(pat.span(), "tool parameters must be plain identifiers"));
        };
        params.push(Param { ident: ident.ident.clone(), ty: (**ty).clone(), doc: doc_of(attrs) });
    }

    // Strip the parameter doc comments before re-emitting: they have done their job, and Rust does
    // not accept doc comments on function parameters in all positions.
    let mut inner = function.clone();
    for input in &mut inner.sig.inputs {
        if let FnArg::Typed(pt) = input {
            pt.attrs.retain(|a| !a.path().is_ident("doc"));
        }
    }

    let args_struct = format_ident!("{}Args", to_pascal(&fn_name.to_string()));
    let tool_struct = format_ident!("{}Tool", to_pascal(&fn_name.to_string()));

    let field_defs = params.iter().map(|p| {
        let ident = &p.ident;
        let ty = &p.ty;
        match &p.doc {
            Some(doc) => quote! { #[doc = #doc] pub #ident: #ty, },
            None => quote! { pub #ident: #ty, },
        }
    });
    let field_names: Vec<_> = params.iter().map(|p| &p.ident).collect();

    let tool_name = fn_name.to_string();
    let capabilities = &options.capabilities;
    let cost_hint = options.cost_hint.as_deref().unwrap_or("cheap");
    let caller = options.caller.as_deref().unwrap_or("direct");
    let presentation = options.presentation.as_deref().unwrap_or("deferred");

    Ok(quote! {
        #inner

        #[doc = "Arguments for the `"]
        #[doc = #tool_name]
        #[doc = "` tool, generated by `#[frey::tool]`."]
        #[derive(Debug, ::frey_tools::__private::serde::Deserialize, ::frey_tools::__private::schemars::JsonSchema)]
        #[serde(crate = "::frey_tools::__private::serde")]
        #[schemars(crate = "::frey_tools::__private::schemars")]
        #vis struct #args_struct {
            #(#field_defs)*
        }

        #[doc = "The `"]
        #[doc = #tool_name]
        #[doc = "` tool, generated by `#[frey::tool]`."]
        #[derive(Debug, Clone, Copy, Default)]
        #vis struct #tool_struct;

        impl #tool_struct {
            /// The generated definition, including the schema built from the parameter types and
            /// their doc comments.
            pub fn definition() -> ::frey_tools::__private::ToolDefinition {
                ::frey_tools::__private::build_definition(
                    #tool_name,
                    #description,
                    ::frey_tools::__private::schema_for::<#args_struct>(),
                    &[#(#capabilities),*],
                    #cost_hint,
                    #caller,
                    #presentation,
                )
            }

            /// Decode arguments and run the function.
            pub async fn invoke(
                args: ::frey_tools::__private::Value,
            ) -> ::core::result::Result<
                ::frey_tools::__private::ToolContent,
                ::frey_tools::__private::ToolError,
            > {
                let parsed: #args_struct =
                    ::frey_tools::__private::decode_args(args, #tool_name)?;
                let output = #fn_name(#(parsed.#field_names),*).await?;
                ::core::result::Result::Ok(::frey_tools::__private::into_content(output))
            }
        }
    })
}

fn to_pascal(snake: &str) -> String {
    snake
        .split('_')
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_becomes_pascal_case() {
        assert_eq!(to_pascal("fs_read"), "FsRead");
        assert_eq!(to_pascal("get_weather_now"), "GetWeatherNow");
        assert_eq!(to_pascal("single"), "Single");
        assert_eq!(to_pascal("trailing_"), "Trailing");
    }

    #[test]
    fn doc_comments_are_joined_and_trimmed() {
        let attrs: Vec<Attribute> = syn::parse_quote! {
            /// First line.
            /// Second line.
        };
        assert_eq!(doc_of(&attrs).as_deref(), Some("First line. Second line."));
    }

    #[test]
    fn an_absent_doc_comment_is_none_rather_than_an_empty_string() {
        let attrs: Vec<Attribute> = syn::parse_quote! { #[inline] };
        assert_eq!(doc_of(&attrs), None);
    }

    #[test]
    fn a_missing_description_is_an_error_that_explains_why_it_matters() {
        let function: ItemFn = syn::parse_quote! {
            async fn undocumented(x: String) -> Result<String, ToolError> { Ok(x) }
        };
        let args = Punctuated::new();
        let err = expand(&args, &function).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("description"), "{message}");
        assert!(message.contains("Tool search"), "the error must say why: {message}");
    }

    #[test]
    fn an_unknown_option_lists_the_valid_ones() {
        let args: Punctuated<Meta, Token![,]> = syn::parse_quote! { descriptoin = "typo" };
        let err = parse_options(&args).unwrap_err();
        assert!(err.to_string().contains("cost_hint"), "{}", err);
    }

    #[test]
    fn capabilities_are_collected_from_a_list() {
        let args: Punctuated<Meta, Token![,]> =
            syn::parse_quote! { capabilities("fs:read", "net:egress"), cost_hint = "destructive" };
        let options = parse_options(&args).unwrap();
        assert_eq!(options.capabilities, vec!["fs:read", "net:egress"]);
        assert_eq!(options.cost_hint.as_deref(), Some("destructive"));
    }
}
