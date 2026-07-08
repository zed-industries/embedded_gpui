//! Proc macros for `embedded_gpui`. See [`macro@shared_interface`].

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::{Expr, FnArg, Ident, ItemTrait, LitStr, MetaNameValue, Token, TraitItem, Type};

/// Turns a trait into a complete shared-entity interface.
///
/// ```ignore
/// #[shared_interface(spec = CommandSpec, type_name = "demo.command", snapshot = CommandSnapshot)]
/// pub trait CommandApi {
///     fn invoke(&mut self, cx: &mut gpui::Context<Self>) -> String;
/// }
/// ```
///
/// generates, alongside the trait itself (which becomes the *home*-side handler trait):
///
/// - `pub struct CommandSpec` implementing `SharedSpec` with the given snapshot type;
/// - one message struct per method (`Invoke { ... }`), named by PascalCasing the method,
///   with `SharedMessage` wired to the method's return type;
/// - `pub trait CommandApiCaller: SharedCaller<CommandSpec>` with a default method per
///   trait method returning a `CallReceipt`, blanket-implemented for every caller — so
///   both `Remote<CommandSpec>` and `HostRemote<CommandSpec>` get `.invoke(cx)`;
/// - `pub fn register_command_api(&mut Methods<..>)` installing decode-dispatch-encode
///   handlers that forward to the trait implementation.
///
/// Method shape: `&mut self`, any number of serde-serializable arguments, and a final
/// `cx: &mut Context<Self>` parameter. Async handlers still register manually via
/// `Methods::on_async`.
#[proc_macro_attribute]
pub fn shared_interface(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(
        attr with Punctuated::<MetaNameValue, Token![,]>::parse_terminated
    );
    let item_trait = syn::parse_macro_input!(item as ItemTrait);
    match expand(args, item_trait) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

struct Method {
    ident: Ident,
    message_ident: Ident,
    method_name: String,
    field_names: Vec<Ident>,
    field_types: Vec<Type>,
    response: proc_macro2::TokenStream,
}

fn expand(
    args: Punctuated<MetaNameValue, Token![,]>,
    mut item_trait: ItemTrait,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut spec: Option<Ident> = None;
    let mut type_name: Option<LitStr> = None;
    let mut snapshot: Option<Expr> = None;
    for arg in &args {
        let key = arg
            .path
            .get_ident()
            .map(Ident::to_string)
            .unwrap_or_default();
        match (key.as_str(), &arg.value) {
            ("spec", Expr::Path(path)) => {
                spec = path.path.get_ident().cloned();
            }
            ("type_name", Expr::Lit(lit)) => {
                if let syn::Lit::Str(lit) = &lit.lit {
                    type_name = Some(lit.clone());
                }
            }
            ("snapshot", expr) => snapshot = Some((*expr).clone()),
            _ => {
                return Err(syn::Error::new(
                    arg.span(),
                    "expected `spec = Ident`, `type_name = \"...\"`, or `snapshot = Type`",
                ));
            }
        }
    }
    let spec = spec.ok_or_else(|| syn::Error::new(args.span(), "missing `spec = Ident`"))?;
    let type_name =
        type_name.ok_or_else(|| syn::Error::new(args.span(), "missing `type_name = \"...\"`"))?;
    let snapshot =
        snapshot.ok_or_else(|| syn::Error::new(args.span(), "missing `snapshot = Type`"))?;

    let vis = item_trait.vis.clone();
    let trait_ident = item_trait.ident.clone();

    let mut methods = Vec::new();
    for item in &item_trait.items {
        let TraitItem::Fn(function) = item else {
            continue;
        };
        let sig = &function.sig;
        if sig.asyncness.is_some() {
            return Err(syn::Error::new(
                sig.span(),
                "async methods are not supported; register with Methods::on_async instead",
            ));
        }
        let mut inputs = sig.inputs.iter();
        match inputs.next() {
            Some(FnArg::Receiver(receiver)) if receiver.mutability.is_some() => {}
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "shared interface methods take `&mut self`",
                ));
            }
        }
        let mut typed: Vec<_> = inputs.collect();
        // The final parameter is the context; everything between self and it becomes
        // message fields.
        if typed.pop().is_none() {
            return Err(syn::Error::new(
                sig.span(),
                "shared interface methods end with `cx: &mut Context<Self>`",
            ));
        }
        let mut field_names = Vec::new();
        let mut field_types = Vec::new();
        for arg in typed {
            let FnArg::Typed(arg) = arg else {
                return Err(syn::Error::new(arg.span(), "unexpected receiver"));
            };
            let syn::Pat::Ident(pat) = &*arg.pat else {
                return Err(syn::Error::new(
                    arg.pat.span(),
                    "message parameters must be plain identifiers",
                ));
            };
            field_names.push(pat.ident.clone());
            field_types.push((*arg.ty).clone());
        }
        let response = match &sig.output {
            syn::ReturnType::Default => quote!(()),
            syn::ReturnType::Type(_, ty) => quote!(#ty),
        };
        let method_name = sig.ident.to_string();
        methods.push(Method {
            ident: sig.ident.clone(),
            message_ident: format_ident!("{}", pascal_case(&method_name), span = sig.ident.span()),
            method_name,
            field_names,
            field_types,
            response,
        });
    }

    // The trait is the home-side handler surface; its methods use Context<Self>.
    item_trait.supertraits.push(syn::parse_quote!('static));
    item_trait.supertraits.push(syn::parse_quote!(Sized));

    let caller_ident = format_ident!("{trait_ident}Caller");
    let register_ident = format_ident!("register_{}", snake_case(&trait_ident.to_string()));

    let message_items = methods.iter().map(|method| {
        let Method { message_ident, method_name, field_names, field_types, response, .. } = method;
        quote! {
            #[derive(Clone, Debug, embedded_gpui::serde::Serialize, embedded_gpui::serde::Deserialize)]
            #[serde(crate = "embedded_gpui::serde")]
            #vis struct #message_ident {
                #(pub #field_names: #field_types,)*
            }

            impl embedded_gpui::SharedMessage for #message_ident {
                type Spec = #spec;
                type Response = #response;
                const METHOD: &'static str = #method_name;
            }
        }
    });

    let caller_methods = methods.iter().map(|method| {
        let Method {
            ident,
            message_ident,
            field_names,
            field_types,
            response,
            ..
        } = method;
        quote! {
            fn #ident(
                &self,
                #(#field_names: #field_types,)*
                cx: &mut embedded_gpui::gpui::App,
            ) -> embedded_gpui::CallReceipt<#response> {
                self.call_shared(#message_ident { #(#field_names,)* }, cx)
            }
        }
    });

    let registrations = methods.iter().map(|method| {
        let Method {
            ident,
            message_ident,
            method_name,
            field_names,
            ..
        } = method;
        quote! {
            methods.on_raw(#method_name, |entity, _method, payload, cx| {
                let message: #message_ident = embedded_gpui::decode(payload)?;
                let response = entity.update(cx, |this, cx| {
                    <T as #trait_ident>::#ident(this, #(message.#field_names,)* cx)
                });
                embedded_gpui::encode(&response)
            });
        }
    });

    let trait_doc = format!(
        "Typed calls to a shared `{spec}` entity, for any holder of a capability to one \
         ([`SharedCaller`](embedded_gpui::SharedCaller)): guest `Remote` and host \
         `HostRemote` alike."
    );
    let register_doc = format!(
        "Installs the `{trait_ident}` methods of `T` into a shared entity's dispatch table."
    );

    Ok(quote! {
        #item_trait

        #vis struct #spec;

        impl embedded_gpui::SharedSpec for #spec {
            const TYPE_NAME: &'static str = #type_name;
            type Snapshot = #snapshot;
        }

        #(#message_items)*

        #[doc = #trait_doc]
        #vis trait #caller_ident: embedded_gpui::SharedCaller<#spec> {
            #(#caller_methods)*
        }

        impl<C: embedded_gpui::SharedCaller<#spec>> #caller_ident for C {}

        #[doc = #register_doc]
        #vis fn #register_ident<T>(methods: &mut embedded_gpui::Methods<#spec, T>)
        where
            T: #trait_ident + 'static,
        {
            #(#registrations)*
        }
    })
}

fn pascal_case(snake: &str) -> String {
    snake
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn snake_case(pascal: &str) -> String {
    let mut out = String::new();
    for (index, ch) in pascal.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
