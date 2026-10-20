#[unsafe(no_mangle)]
pub extern "C" fn fx_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len);
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}

/// # Safety
/// ptr/len from fx_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fx_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
    }
}

fn pack(v: Vec<u8>) -> u64 {
    let b = v.into_boxed_slice();
    let len = b.len() as u64;
    ((Box::into_raw(b) as *mut u8 as usize as u64) << 32) | len
}

/// # Safety
/// ptr/len from fx_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fx_echo(ptr: *const u8, len: usize) -> u64 {
    let input = unsafe { core::slice::from_raw_parts(ptr, len) };
    #[cfg(not(feature = "json"))]
    let out = input.to_vec();
    #[cfg(feature = "json")]
    let out = {
        #[derive(serde::Deserialize, serde::Serialize)]
        struct In { url: String, base: Option<String>, n: Option<f64> }
        match serde_json::from_slice::<In>(input) {
            Ok(mut v) => {
                #[cfg(feature = "url")]
                if let Some(b) = &v.base {
                    if let Ok(j) = url::Url::parse(b).and_then(|b| b.join(&v.url)) {
                        v.url = j.into();
                    }
                }
                serde_json::to_vec(&v).unwrap_or_default()
            }
            Err(e) => e.to_string().into_bytes(),
        }
    };
    pack(out)
}
