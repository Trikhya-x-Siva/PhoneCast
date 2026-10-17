//! Small secrets (the Twitch token) encrypted with the Windows account
//! (DPAPI), same as the app's stream keys. Plain text on other platforms.

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    fn call(data: &[u8], protect: bool) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &input,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    &mut out,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    &mut out,
                )
            }
        };
        if ok == 0 {
            return Err("could not unlock the saved login; please log in again".into());
        }
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        Ok(v)
    }

    pub fn protect(s: &str) -> Result<String, String> {
        Ok(crate::models::hex(&call(s.as_bytes(), true)?))
    }

    pub fn unprotect(hex: &str) -> Result<String, String> {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect();
        String::from_utf8(call(&bytes, false)?).map_err(|e| e.to_string())
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn protect(s: &str) -> Result<String, String> {
        Ok(s.into())
    }
    pub fn unprotect(s: &str) -> Result<String, String> {
        Ok(s.into())
    }
}

pub use imp::{protect, unprotect};

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let enc = super::protect("hello").unwrap();
        assert_eq!(super::unprotect(&enc).unwrap(), "hello");
    }
}
