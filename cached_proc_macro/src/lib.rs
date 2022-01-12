use darling::FromMeta;
use proc_macro::TokenStream;
use quote::quote;
use std::ops::Deref;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    parse_macro_input, parse_str, AttributeArgs, Block, FnArg, Ident, ItemFn, Pat, PathArguments,
    ReturnType, Type,
};

#[derive(FromMeta)]
struct MacroArgs {
    #[darling(default)]
    name: Option<String>,
    #[darling(default)]
    unbound: bool,
    #[darling(default)]
    size: Option<usize>,
    #[darling(default)]
    time: Option<u64>,
    #[darling(default)]
    time_refresh: bool,
    #[darling(default)]
    key: Option<String>,
    #[darling(default)]
    convert: Option<String>,
    #[darling(default)]
    result: bool,
    #[darling(default)]
    option: bool,
    #[darling(default)]
    sync_writes: bool,
    #[darling(default)]
    with_cached_flag: bool,
    #[darling(default, rename = "type")]
    cache_type: Option<String>,
    #[darling(default, rename = "create")]
    cache_create: Option<String>,
}

/// # Attributes
/// - **Cache Name:** Use `name = "CACHE_NAME"` to specify the name for the generated cache.
/// - **Cache Type:** The default cache type is `UnboundCache`.
/// You specify which of the built-in cache types to use with `unbound`, `size = cache_size`, and `time = lifetime_in_seconds`
/// - **Cache Create:** You can specify the cache creation with `create = "{ CacheType::new() }"`.
/// - **Custom Cache Type:** You can use `type = "CacheType"` to specify the type of cache to use.
/// This requires create to also be set.
/// - **Cache Key:** Use `key = "KeyType"` to specify what type to use for the cache key.
/// This requires convert to also be set.
/// - **Cache Key Convert:** Use `convert = "{ convert_inputs_to_key }"`.
/// This requires either key or type to also be set.
/// - **Caching Result/Option:** If your function returns a `Result` or `Option`
/// you may want to use `result` or `option` to only cache when the output is `Ok` or `Some`
/// - ** Indicating whether a result was cached ** Use `with_cached_flag = true` with a
/// `cached::Return<T>` return type to have the macro set the `was_cached` flag on the
/// `cached::Return<T>` result.
/// ## Note
/// The `type`, `create`, `key`, and `convert` attributes must be in a `String`
/// This is because darling, which is used for parsing the attributes, does not support parsing attributes into `Type` or `Block`.
#[proc_macro_attribute]
pub fn cached(args: TokenStream, input: TokenStream) -> TokenStream {
    let attr_args = parse_macro_input!(args as AttributeArgs);
    let args = match MacroArgs::from_list(&attr_args) {
        Ok(v) => v,
        Err(e) => {
            return TokenStream::from(e.write_errors());
        }
    };
    let input = parse_macro_input!(input as ItemFn);

    // pull out the parts of the input
    let _attributes = input.attrs;
    let visibility = input.vis;
    let signature = input.sig;
    let body = input.block;

    // pull out the parts of the function signature
    let fn_ident = signature.ident.clone();
    let inputs = signature.inputs.clone();
    let output = signature.output.clone();
    let asyncness = signature.asyncness;

    // pull out the names and types of the function inputs
    let input_tys = inputs
        .iter()
        .map(|input| match input {
            FnArg::Receiver(_) => panic!("methods (functions taking 'self') are not supported"),
            FnArg::Typed(pat_type) => pat_type.ty.clone(),
        })
        .collect::<Vec<Box<Type>>>();

    let input_names = inputs
        .iter()
        .map(|input| match input {
            FnArg::Receiver(_) => panic!("methods (functions taking 'self') are not supported"),
            FnArg::Typed(pat_type) => {
                // if you define arguments as mutable, e.g.
                // #[cached]
                // fn mutable_args(mut a: i32, mut b: i32) -> (i32, i32) {
                //     a += 1;
                //     b += 1;
                //     (a, b)
                // }
                // then we need to strip off the `mut` keyword from the
                // variable identifiers so we can refer to arguments `a` and `b`
                // instead of `mut a` and `mut b`
                match &pat_type.pat.deref() {
                    Pat::Ident(pat_ident) => {
                        if pat_ident.mutability.is_some() {
                            let mut p = pat_ident.clone();
                            p.mutability = None;
                            Box::new(Pat::Ident(p))
                        } else {
                            Box::new(Pat::Ident(pat_ident.clone()))
                        }
                    }
                    _ => pat_type.pat.clone(),
                }
            }
        })
        .collect::<Vec<Box<Pat>>>();

    // pull out the output type
    let output_ty = match &output {
        ReturnType::Default => quote! {()},
        ReturnType::Type(_, ty) => quote! {#ty},
    };

    let output_span = output_ty.span();
    let output_ts = TokenStream::from(output_ty.clone());
    let output_parts = output_ts
        .clone()
        .into_iter()
        .filter_map(|tt| match tt {
            proc_macro::TokenTree::Ident(ident) => Some(ident.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let output_string = output_parts.join("::");
    let output_type_display = output_ts.to_string().replace(" ", "");

    // if `with_cached_flag = true`, then enforce that the return type
    // is something wrapped in `Return`. Either `Return<T>` or the
    // fully qualified `cached::Return<T>`
    if args.with_cached_flag
        && !output_string.contains("Return")
        && !output_string.contains("cached::Return")
    {
        return syn::Error::new(
            output_span,
            format!(
                "\nWhen specifying `with_cached_flag = true`, \
                    the return type must be wrapped in `cached::Return<T>`. \n\
                    The following return types are supported: \n\
                    |    `cached::Return<T>`\n\
                    |    `std::result::Result<cachedReturn<T>, E>`\n\
                    |    `std::option::Option<cachedReturn<T>>`\n\
                    Found type: {t}.",
                t = output_type_display
            ),
        )
        .to_compile_error()
        .into();
    }

    // Find the type of the value to store.
    // Normally it's the same as the return type of the functions, but
    // for Options and Results it's the (first) inner type. So for
    // Option<u32>, store u32, for Result<i32, String>, store i32, etc.
    let cache_value_ty = match (&args.result, &args.option) {
        (false, false) => output_ty,
        (true, true) => panic!("the result and option attributes are mutually exclusive"),
        _ => match output.clone() {
            ReturnType::Default => {
                panic!("function must return something for result or option attributes")
            }
            ReturnType::Type(_, ty) => {
                if let Type::Path(typepath) = *ty {
                    let segments = typepath.path.segments;
                    if let PathArguments::AngleBracketed(brackets) =
                        &segments.last().unwrap().arguments
                    {
                        let inner_ty = brackets.args.first().unwrap();
                        quote! {#inner_ty}
                    } else {
                        panic!("function return type has no inner type")
                    }
                } else {
                    panic!("function return type too complex")
                }
            }
        },
    };

    // make the cache identifier
    let cache_ident = match args.name {
        Some(name) => Ident::new(&name, fn_ident.span()),
        None => Ident::new(&fn_ident.to_string().to_uppercase(), fn_ident.span()),
    };

    // make the cache key type and block that converts the inputs into the key type
    let (cache_key_ty, key_convert_block) = match (&args.key, &args.convert, &args.cache_type) {
        (Some(key_str), Some(convert_str), _) => {
            let cache_key_ty = parse_str::<Type>(key_str).expect("unable to parse cache key type");

            let key_convert_block =
                parse_str::<Block>(convert_str).expect("unable to parse key convert block");

            (quote! {#cache_key_ty}, quote! {#key_convert_block})
        }
        (None, Some(convert_str), Some(_)) => {
            let key_convert_block =
                parse_str::<Block>(convert_str).expect("unable to parse key convert block");

            (quote! {}, quote! {#key_convert_block})
        }
        (None, None, _) => (
            quote! {(#(#input_tys),*)},
            quote! {(#(#input_names.clone()),*)},
        ),
        (Some(_), None, _) => panic!("key requires convert to be set"),
        (None, Some(_), None) => panic!("convert requires key or type to be set"),
    };

    // make the cache type and create statement
    let (cache_ty, cache_create) = match (
        &args.unbound,
        &args.size,
        &args.time,
        &args.cache_type,
        &args.cache_create,
        &args.time_refresh,
    ) {
        (true, None, None, None, None, _) => {
            let cache_ty = quote! {cached::UnboundCache<#cache_key_ty, #cache_value_ty>};
            let cache_create = quote! {cached::UnboundCache::new()};
            (cache_ty, cache_create)
        }
        (false, Some(size), None, None, None, _) => {
            let cache_ty = quote! {cached::SizedCache<#cache_key_ty, #cache_value_ty>};
            let cache_create = quote! {cached::SizedCache::with_size(#size)};
            (cache_ty, cache_create)
        }
        (false, None, Some(time), None, None, time_refresh) => {
            let cache_ty = quote! {cached::TimedCache<#cache_key_ty, #cache_value_ty>};
            let cache_create =
                quote! {cached::TimedCache::with_lifespan_and_refresh(#time, #time_refresh)};
            (cache_ty, cache_create)
        }
        (false, Some(size), Some(time), None, None, time_refresh) => {
            let cache_ty = quote! {cached::TimedSizedCache<#cache_key_ty, #cache_value_ty>};
            let cache_create = quote! {cached::TimedSizedCache::with_size_and_lifespan_and_refresh(#size, #time, #time_refresh)};
            (cache_ty, cache_create)
        }
        (false, None, None, None, None, _) => {
            let cache_ty = quote! {cached::UnboundCache<#cache_key_ty, #cache_value_ty>};
            let cache_create = quote! {cached::UnboundCache::new()};
            (cache_ty, cache_create)
        }
        (false, None, None, Some(type_str), Some(create_str), _) => {
            let cache_type = parse_str::<Type>(type_str).expect("unable to parse cache type");

            let cache_create =
                parse_str::<Block>(create_str).expect("unable to parse cache create block");

            (quote! { #cache_type }, quote! { #cache_create })
        }
        (false, None, None, Some(_), None, _) => panic!("type requires create to also be set"),
        (false, None, None, None, Some(_), _) => panic!("create requires type to also be set"),
        _ => panic!(
            "cache types (unbound, size and/or time, or type and create) are mutually exclusive"
        ),
    };

    // make the set cache and return cache blocks
    let (set_cache_block, return_cache_block) = match (&args.result, &args.option) {
        (false, false) => {
            let set_cache_block = quote! { cache.cache_set(key, result.clone()); };
            let return_cache_block = if args.with_cached_flag {
                quote! { let mut r = result.clone(); r.was_cached = true; return r }
            } else {
                quote! { return result.clone() }
            };
            (set_cache_block, return_cache_block)
        }
        (true, false) => {
            let set_cache_block = quote! {
                if let Ok(result) = &result {
                    cache.cache_set(key, result.clone());
                }
            };
            let return_cache_block = if args.with_cached_flag {
                quote! { let mut r = result.clone(); r.was_cached = true; return Ok(r) }
            } else {
                quote! { return Ok(result.clone()) }
            };
            (set_cache_block, return_cache_block)
        }
        (false, true) => {
            let set_cache_block = quote! {
                if let Some(result) = &result {
                    cache.cache_set(key, result.clone());
                }
            };
            let return_cache_block = if args.with_cached_flag {
                quote! { let mut r = result.clone(); r.was_cached = true; return Some(r) }
            } else {
                quote! { return Some(result.clone()) }
            };
            (set_cache_block, return_cache_block)
        }
        _ => panic!("the result and option attributes are mutually exclusive"),
    };

    let do_set_return_block = if asyncness.is_some() {
        if args.sync_writes {
            quote! {
                // try to get a write lock first
                let mut cache = #cache_ident.lock().await;
                if let Some(result) = cache.cache_get(&key) {
                    #return_cache_block
                }

                // run the function and cache the result
                async fn inner(#inputs) #output #body;
                let result = inner(#(#input_names),*).await;
                #set_cache_block
                result
            }
        } else {
            quote! {
                // run the function and cache the result
                async fn inner(#inputs) #output #body;
                let result = inner(#(#input_names),*).await;
                let mut cache = #cache_ident.lock().await;
                #set_cache_block
                result
            }
        }
    } else if args.sync_writes {
        quote! {
            // try to get a write lock first
            let mut cache = #cache_ident.lock().unwrap();
            if let Some(result) = cache.cache_get(&key) {
                #return_cache_block
            }

            // run the function and cache the result
            fn inner(#inputs) #output #body;
            let result = inner(#(#input_names),*);
            #set_cache_block
            result
        }
    } else {
        quote! {
            // run the function and cache the result
            fn inner(#inputs) #output #body;
            let result = inner(#(#input_names),*);
            let mut cache = #cache_ident.lock().unwrap();
            #set_cache_block
            result
        }
    };

    // if you define arguments as mutable, e.g.
    // #[cached]
    // fn mutable_args(mut a: i32, mut b: i32) -> (i32, i32) {
    //     a += 1;
    //     b += 1;
    //     (a, b)
    // }
    // then we want the `mut` keywords present on the "inner" function
    // that wraps your actual block of code.
    // If the `mut`s are also on the outer method, then you'll
    // get compiler warnings about your arguments not needing to be `mut`
    // when they really do need to be.
    let mut signature_no_muts = signature;
    let mut sig_inputs = Punctuated::new();
    for inp in &signature_no_muts.inputs {
        let item = match inp {
            FnArg::Receiver(_) => inp.clone(),
            FnArg::Typed(pat_type) => {
                let mut pt = pat_type.clone();
                let pat = match &pat_type.pat.deref() {
                    Pat::Ident(pat_ident) => {
                        if pat_ident.mutability.is_some() {
                            let mut p = pat_ident.clone();
                            p.mutability = None;
                            Box::new(Pat::Ident(p))
                        } else {
                            Box::new(Pat::Ident(pat_ident.clone()))
                        }
                    }
                    _ => pat_type.pat.clone(),
                };
                pt.pat = pat;
                FnArg::Typed(pt)
            }
        };
        sig_inputs.push(item);
    }
    signature_no_muts.inputs = sig_inputs;

    // create a signature for the cache-priming function
    let prime_fn_ident = Ident::new(&format!("{}_prime_cache", &fn_ident), fn_ident.span());
    let mut prime_sig = signature_no_muts.clone();
    prime_sig.ident = prime_fn_ident;

    let prime_do_set_return_block = if asyncness.is_some() {
        quote! {
            // run the function and cache the result
            async fn inner(#inputs) #output #body;
            let result = inner(#(#input_names),*).await;
            let mut cache = #cache_ident.lock().await;
            #set_cache_block
            result
        }
    } else {
        quote! {
            // run the function and cache the result
            fn inner(#inputs) #output #body;
            let result = inner(#(#input_names),*);
            let mut cache = #cache_ident.lock().unwrap();
            #set_cache_block
            result
        }
    };

    // put it all together
    let expanded = if asyncness.is_some() {
        quote! {
            /// Cached static
            #visibility static #cache_ident: ::cached::once_cell::sync::Lazy<::cached::async_mutex::Mutex<#cache_ty>> = ::cached::once_cell::sync::Lazy::new(|| ::cached::async_mutex::Mutex::new(#cache_create));
            /// Cached function
            #visibility #signature_no_muts {
                use cached::Cached;
                let key = #key_convert_block;
                {
                    // check if the result is cached
                    let mut cache = #cache_ident.lock().await;
                    if let Some(result) = cache.cache_get(&key) {
                        #return_cache_block
                    }
                }
                #do_set_return_block
            }
            /// Prime cached function
            #visibility #prime_sig {
                use cached::Cached;
                let key = #key_convert_block;
                #prime_do_set_return_block
            }
        }
    } else {
        quote! {
            /// Cached static
            #visibility static #cache_ident: ::cached::once_cell::sync::Lazy<std::sync::Mutex<#cache_ty>> = ::cached::once_cell::sync::Lazy::new(|| std::sync::Mutex::new(#cache_create));
            /// Cached function
            #visibility #signature_no_muts {
                use cached::Cached;
                let key = #key_convert_block;
                {
                    // check if the result is cached
                    let mut cache = #cache_ident.lock().unwrap();
                    if let Some(result) = cache.cache_get(&key) {
                        #return_cache_block
                    }
                }
                #do_set_return_block
            }
            /// Prime cached function
            #visibility #prime_sig {
                use cached::Cached;
                let key = #key_convert_block;
                #prime_do_set_return_block
            }
        }
    };

    expanded.into()
}

#[derive(FromMeta)]
struct OnceMacroArgs {
    #[darling(default)]
    name: Option<String>,
    #[darling(default)]
    time: Option<u64>,
    #[darling(default)]
    sync_writes: bool,
    #[darling(default)]
    result: bool,
    #[darling(default)]
    option: bool,
    #[darling(default)]
    with_cached_flag: bool,
}

/// # Attributes
/// - **Cache Name:** Use `name = "CACHE_NAME"` to specify the name for the generated cache.
/// - **Caching Result/Option:** If your function returns a `Result` or `Option`
/// you may want to use `result` or `option` to only cache when the output is `Ok` or `Some`
/// - ** Indicating whether a result was cached ** Use `with_cached_flag = true` with a
/// `cached::Return<T>` return type to have the macro set the `was_cached` flag on the
/// `cached::Return<T>` result.
#[proc_macro_attribute]
pub fn once(args: TokenStream, input: TokenStream) -> TokenStream {
    let attr_args = parse_macro_input!(args as AttributeArgs);
    let args = match OnceMacroArgs::from_list(&attr_args) {
        Ok(v) => v,
        Err(e) => {
            return TokenStream::from(e.write_errors());
        }
    };
    let input = parse_macro_input!(input as ItemFn);

    // pull out the parts of the input
    let _attributes = input.attrs;
    let visibility = input.vis;
    let signature = input.sig;
    let body = input.block;

    // pull out the parts of the function signature
    let fn_ident = signature.ident.clone();
    let inputs = signature.inputs.clone();
    let output = signature.output.clone();
    let asyncness = signature.asyncness;

    // pull out the names and types of the function inputs
    let input_names = inputs
        .iter()
        .map(|input| match input {
            FnArg::Receiver(_) => panic!("methods (functions taking 'self') are not supported"),
            FnArg::Typed(pat_type) => {
                // if you define arguments as mutable, e.g.
                // #[once]
                // fn mutable_args(mut a: i32, mut b: i32) -> (i32, i32) {
                //     a += 1;
                //     b += 1;
                //     (a, b)
                // }
                // then we need to strip off the `mut` keyword from the
                // variable identifiers so we can refer to arguments `a` and `b`
                // instead of `mut a` and `mut b`
                match &pat_type.pat.deref() {
                    Pat::Ident(pat_ident) => {
                        if pat_ident.mutability.is_some() {
                            let mut p = pat_ident.clone();
                            p.mutability = None;
                            Box::new(Pat::Ident(p))
                        } else {
                            Box::new(Pat::Ident(pat_ident.clone()))
                        }
                    }
                    _ => pat_type.pat.clone(),
                }
            }
        })
        .collect::<Vec<Box<Pat>>>();

    // pull out the output type
    let output_ty = match &output {
        ReturnType::Default => quote! {()},
        ReturnType::Type(_, ty) => quote! {#ty},
    };

    let output_span = output_ty.span();
    let output_ts = TokenStream::from(output_ty.clone());
    let output_parts = output_ts
        .clone()
        .into_iter()
        .filter_map(|tt| match tt {
            proc_macro::TokenTree::Ident(ident) => Some(ident.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let output_string = output_parts.join("::");
    let output_type_display = output_ts.to_string().replace(" ", "");

    // if `with_cached_flag = true`, then enforce that the return type
    // is something wrapped in `Return`. Either `Return<T>` or the
    // fully qualified `cached::Return<T>`
    if args.with_cached_flag
        && !output_string.contains("Return")
        && !output_string.contains("cached::Return")
    {
        return syn::Error::new(
            output_span,
            format!(
                "\nWhen specifying `with_cached_flag = true`, \
                    the return type must be wrapped in `cached::Return<T>`. \n\
                    The following return types are supported: \n\
                    |    `cached::Return<T>`\n\
                    |    `std::result::Result<cachedReturn<T>, E>`\n\
                    |    `std::option::Option<cachedReturn<T>>`\n\
                    Found type: {t}.",
                t = output_type_display
            ),
        )
        .to_compile_error()
        .into();
    }

    // Find the type of the value to store.
    // Normally it's the same as the return type of the functions, but
    // for Options and Results it's the (first) inner type. So for
    // Option<u32>, store u32, for Result<i32, String>, store i32, etc.
    let cache_value_ty = match (&args.result, &args.option) {
        (false, false) => output_ty,
        (true, true) => panic!("the result and option attributes are mutually exclusive"),
        _ => match output.clone() {
            ReturnType::Default => {
                panic!("function must return something for result or option attributes")
            }
            ReturnType::Type(_, ty) => {
                if let Type::Path(typepath) = *ty {
                    let segments = typepath.path.segments;
                    if let PathArguments::AngleBracketed(brackets) =
                        &segments.last().unwrap().arguments
                    {
                        let inner_ty = brackets.args.first().unwrap();
                        quote! {#inner_ty}
                    } else {
                        panic!("function return type has no inner type")
                    }
                } else {
                    panic!("function return type too complex")
                }
            }
        },
    };

    // make the cache identifier
    let cache_ident = match args.name {
        Some(name) => Ident::new(&name, fn_ident.span()),
        None => Ident::new(&fn_ident.to_string().to_uppercase(), fn_ident.span()),
    };

    // make the cache type and create statement
    let (cache_ty, cache_create) = match &args.time {
        None => (quote! { Option<#cache_value_ty> }, quote! { None }),
        Some(_) => (
            quote! { Option<(std::time::Instant, #cache_value_ty)> },
            quote! { None },
        ),
    };

    // make the set cache and return cache blocks
    let (set_cache_block, return_cache_block) = match (&args.result, &args.option) {
        (false, false) => {
            let set_cache_block = if args.time.is_some() {
                quote! {
                    *cached = Some((now, result.clone()));
                }
            } else {
                quote! {
                    *cached = Some(result.clone());
                }
            };

            let return_cache_block = if args.with_cached_flag {
                quote! { let mut r = result.clone(); r.was_cached = true; return r }
            } else {
                quote! { return result.clone() }
            };
            let return_cache_block = if let Some(time) = &args.time {
                quote! {
                    let (created_sec, result) = result;
                    if now.duration_since(*created_sec).as_secs() < #time {
                        #return_cache_block
                    }
                }
            } else {
                quote! { #return_cache_block }
            };
            (set_cache_block, return_cache_block)
        }
        (true, false) => {
            let set_cache_block = if args.time.is_some() {
                quote! {
                    if let Ok(result) = &result {
                        *cached = Some((now, result.clone()));
                    }
                }
            } else {
                quote! {
                    if let Ok(result) = &result {
                        *cached = Some(result.clone());
                    }
                }
            };

            let return_cache_block = if args.with_cached_flag {
                quote! { let mut r = result.clone(); r.was_cached = true; return Ok(r) }
            } else {
                quote! { return Ok(result.clone()) }
            };
            let return_cache_block = if let Some(time) = &args.time {
                quote! {
                    let (created_sec, result) = result;
                    if now.duration_since(*created_sec).as_secs() < #time {
                        #return_cache_block
                    }
                }
            } else {
                quote! { #return_cache_block }
            };
            (set_cache_block, return_cache_block)
        }
        (false, true) => {
            let set_cache_block = if args.time.is_some() {
                quote! {
                    if let Some(result) = &result {
                        *cached = Some((now, result.clone()));
                    }
                }
            } else {
                quote! {
                    if let Some(result) = &result {
                        *cached = Some(result.clone());
                    }
                }
            };

            let return_cache_block = if args.with_cached_flag {
                quote! { let mut r = result.clone(); r.was_cached = true; return Some(r) }
            } else {
                quote! { return Some(result.clone()) }
            };
            let return_cache_block = if let Some(time) = &args.time {
                quote! {
                    let (created_sec, result) = result;
                    if now.duration_since(*created_sec).as_secs() < #time {
                        #return_cache_block
                    }
                }
            } else {
                quote! { #return_cache_block }
            };
            (set_cache_block, return_cache_block)
        }
        _ => panic!("the result and option attributes are mutually exclusive"),
    };

    let do_set_return_block = if asyncness.is_some() {
        if args.sync_writes {
            quote! {
                // try to get a write lock first
                let mut cached = #cache_ident.write().await;
                if let Some(result) = &*cached {
                    #return_cache_block
                }

                // run the function and cache the result
                async fn inner(#inputs) #output #body;
                let result = inner(#(#input_names),*).await;
                #set_cache_block
                result
            }
        } else {
            quote! {
                // run the function and cache the result
                async fn inner(#inputs) #output #body;
                let result = inner(#(#input_names),*).await;
                let mut cached = #cache_ident.write().await;
                #set_cache_block
                result
            }
        }
    } else if args.sync_writes {
        quote! {
            // try to get a write lock first
            let mut cached = #cache_ident.write().unwrap();
            if let Some(result) = &*cached {
                #return_cache_block
            }

            // run the function and cache the result
            fn inner(#inputs) #output #body;
            let result = inner(#(#input_names),*);
            #set_cache_block
            result
        }
    } else {
        quote! {
            // run the function and cache the result
            fn inner(#inputs) #output #body;
            let result = inner(#(#input_names),*);
            let mut cached = #cache_ident.write().unwrap();
            #set_cache_block
            result
        }
    };

    // if you define arguments as mutable, e.g.
    // #[once]
    // fn mutable_args(mut a: i32, mut b: i32) -> (i32, i32) {
    //     a += 1;
    //     b += 1;
    //     (a, b)
    // }
    // then we want the `mut` keywords present on the "inner" function
    // that wraps your actual block of code.
    // If the `mut`s are also on the outer method, then you'll
    // get compiler warnings about your arguments not needing to be `mut`
    // when they really do need to be.
    let mut signature_no_muts = signature;
    let mut sig_inputs = Punctuated::new();
    for inp in &signature_no_muts.inputs {
        let item = match inp {
            FnArg::Receiver(_) => inp.clone(),
            FnArg::Typed(pat_type) => {
                let mut pt = pat_type.clone();
                let pat = match &pat_type.pat.deref() {
                    Pat::Ident(pat_ident) => {
                        if pat_ident.mutability.is_some() {
                            let mut p = pat_ident.clone();
                            p.mutability = None;
                            Box::new(Pat::Ident(p))
                        } else {
                            Box::new(Pat::Ident(pat_ident.clone()))
                        }
                    }
                    _ => pat_type.pat.clone(),
                };
                pt.pat = pat;
                FnArg::Typed(pt)
            }
        };
        sig_inputs.push(item);
    }
    signature_no_muts.inputs = sig_inputs;

    let prime_fn_ident = Ident::new(&format!("{}_prime_cache", &fn_ident), fn_ident.span());
    let mut prime_sig = signature_no_muts.clone();
    prime_sig.ident = prime_fn_ident;

    let prime_do_set_return_block = if asyncness.is_some() {
        quote! {
            // run the function and cache the result
            async fn inner(#inputs) #output #body;
            let result = inner(#(#input_names),*).await;
            let mut cached = #cache_ident.write().await;
            #set_cache_block
            result
        }
    } else {
        quote! {
            // run the function and cache the result
            fn inner(#inputs) #output #body;
            let result = inner(#(#input_names),*);
            let mut cached = #cache_ident.write().unwrap();
            #set_cache_block
            result
        }
    };

    // put it all together
    let expanded = if asyncness.is_some() {
        quote! {
            /// Cached static
            #visibility static #cache_ident: ::cached::once_cell::sync::Lazy<::cached::async_rwlock::RwLock<#cache_ty>> = ::cached::once_cell::sync::Lazy::new(|| ::cached::async_rwlock::RwLock::new(#cache_create));
            /// Cached function
            #visibility #signature_no_muts {
                let now = std::time::Instant::now();
                {
                    // check if the result is cached
                    let mut cached = #cache_ident.read().await;
                    if let Some(result) = &*cached {
                        #return_cache_block
                    }
                }
                #do_set_return_block
            }
            /// Prime cached function
            #visibility #prime_sig {
                let now = std::time::Instant::now();
                #prime_do_set_return_block
            }
        }
    } else {
        quote! {
            /// Cached static
            #visibility static #cache_ident: ::cached::once_cell::sync::Lazy<std::sync::RwLock<#cache_ty>> = ::cached::once_cell::sync::Lazy::new(|| std::sync::RwLock::new(#cache_create));
            /// Cached function
            #visibility #signature_no_muts {
                let now = std::time::Instant::now();
                {
                    // check if the result is cached
                    let mut cached = #cache_ident.read().unwrap();
                    if let Some(result) = &*cached {
                        #return_cache_block
                    }
                }
                #do_set_return_block
            }
            /// Prime cached function
            #visibility #prime_sig {
                let now = std::time::Instant::now();
                #prime_do_set_return_block
            }
        }
    };

    expanded.into()
}
