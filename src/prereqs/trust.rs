//! Authenticode verification for downloaded installers: the signature must be
//! valid (WinVerifyTrust), the signing certificate must name Microsoft
//! Corporation, and its chain must end in a Microsoft root.

use crate::i18n::{t, tf};

/// What a passed check found, for the activity line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signer {
    pub(crate) subject: String,
}

/// Whether a leaf certificate's names identify Microsoft as the publisher.
pub(crate) fn is_microsoft_publisher(common_name: &str, organization: &str) -> bool {
    organization.trim() == "Microsoft Corporation" && common_name.trim().starts_with("Microsoft")
}

/// Verifies `path` (opened as `file`, which the caller keeps open with write
/// and delete sharing denied so the bytes cannot change before they run).
#[cfg(windows)]
pub(crate) fn verify_microsoft(path: &std::path::Path, file: &std::fs::File) -> Result<Signer, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Security::Cryptography::{
        CertGetNameStringW, CertVerifyCertificateChainPolicy, CERT_CHAIN_POLICY_MICROSOFT_ROOT,
        CERT_CHAIN_POLICY_PARA, CERT_CHAIN_POLICY_STATUS, CERT_NAME_ATTR_TYPE, CERT_NAME_SIMPLE_DISPLAY_TYPE,
        MICROSOFT_ROOT_CERT_CHAIN_POLICY_CHECK_APPLICATION_ROOT_FLAG, szOID_ORGANIZATION_NAME,
    };
    use windows_sys::Win32::Security::WinTrust::{
        WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData,
        WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_FILE_INFO, WTD_CHOICE_FILE,
        WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT, WTD_REVOKE_WHOLECHAIN, WTD_STATEACTION_CLOSE,
        WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    };

    let wide_path = super::detect::sys::wide_os(path.as_os_str());
    unsafe {
        let mut info = WINTRUST_FILE_INFO {
            cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: wide_path.as_ptr(),
            hFile: file.as_raw_handle() as _,
            pgKnownSubject: std::ptr::null_mut(),
        };
        let mut data: WINTRUST_DATA = std::mem::zeroed();
        data.cbStruct = std::mem::size_of::<WINTRUST_DATA>() as u32;
        data.dwUIChoice = WTD_UI_NONE;
        data.fdwRevocationChecks = WTD_REVOKE_WHOLECHAIN;
        data.dwUnionChoice = WTD_CHOICE_FILE;
        data.Anonymous.pFile = &mut info;
        data.dwStateAction = WTD_STATEACTION_VERIFY;
        data.dwProvFlags = WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT;
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let status = WinVerifyTrust(std::ptr::null_mut(), &mut action, (&mut data as *mut WINTRUST_DATA).cast());

        let result = (|| {
            if status != 0 {
                return Err(tf(
                    "The installer's digital signature is not valid (WinVerifyTrust {code}). It was not run.",
                    &[("code", &format!("0x{:08X}", status as u32))],
                ));
            }
            let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
            if provider.is_null() {
                return Err(t("Could not read the installer's signature.").into());
            }
            let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
            if signer.is_null() || (*signer).pChainContext.is_null() {
                return Err(t("The installer's signature has no signer.").into());
            }
            let leaf = WTHelperGetProvCertFromChain(signer, 0);
            if leaf.is_null() || (*leaf).pCert.is_null() {
                return Err(t("The installer's signature has no certificate.").into());
            }
            let cert = (*leaf).pCert;
            let name = |kind: u32, para: *const core::ffi::c_void| {
                let mut buf = [0u16; 256];
                let len = CertGetNameStringW(cert, kind, 0, para, buf.as_mut_ptr(), buf.len() as u32);
                String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)])
            };
            let common = name(CERT_NAME_SIMPLE_DISPLAY_TYPE, std::ptr::null());
            let org = name(CERT_NAME_ATTR_TYPE, szOID_ORGANIZATION_NAME as *const core::ffi::c_void);
            if !is_microsoft_publisher(&common, &org) {
                return Err(tf(
                    "The installer is signed by \u{201c}{signer}\u{201d}, not Microsoft. It was not run.",
                    &[("signer", &common)],
                ));
            }
            let mut para: CERT_CHAIN_POLICY_PARA = std::mem::zeroed();
            para.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_PARA>() as u32;
            para.dwFlags = MICROSOFT_ROOT_CERT_CHAIN_POLICY_CHECK_APPLICATION_ROOT_FLAG;
            let mut policy: CERT_CHAIN_POLICY_STATUS = std::mem::zeroed();
            policy.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;
            let ok = CertVerifyCertificateChainPolicy(
                CERT_CHAIN_POLICY_MICROSOFT_ROOT,
                (*signer).pChainContext,
                &para,
                &mut policy,
            );
            if ok == 0 || policy.dwError != 0 {
                return Err(tf(
                    "The installer's certificate does not chain to a Microsoft root ({code}). It was not run.",
                    &[("code", &format!("0x{:08X}", policy.dwError))],
                ));
            }
            Ok(Signer { subject: common })
        })();

        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(std::ptr::null_mut(), &mut action, (&mut data as *mut WINTRUST_DATA).cast());
        result
    }
}

#[cfg(not(windows))]
pub(crate) fn verify_microsoft(_path: &std::path::Path, _file: &std::fs::File) -> Result<Signer, String> {
    Err(crate::i18n::t("Signature checks need Windows.").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_names() {
        assert!(is_microsoft_publisher("Microsoft Corporation", "Microsoft Corporation"));
        assert!(!is_microsoft_publisher("Microsoft Corporation", "Contoso"));
        assert!(!is_microsoft_publisher("Contoso", "Microsoft Corporation"));
        assert!(!is_microsoft_publisher("", ""));
    }

    /// The launcher's own unsigned test binary must fail verification.
    #[cfg(windows)]
    #[test]
    fn unsigned_file_is_rejected() {
        let exe = std::env::current_exe().unwrap();
        let file = std::fs::File::open(&exe).unwrap();
        assert!(verify_microsoft(&exe, &file).is_err());
    }

    /// A Microsoft-signed system binary passes (catalog-signed files would not,
    /// so use one with an embedded signature when available).
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn downloaded_installers_verify() {
        // XI_PREREQS_SIGNED=path1;path2 points at downloaded installers.
        let Ok(list) = std::env::var("XI_PREREQS_SIGNED") else { return };
        for path in list.split(';').filter(|p| !p.is_empty()) {
            let path = std::path::Path::new(path);
            let file = std::fs::File::open(path).unwrap();
            let result = verify_microsoft(path, &file);
            println!("{}: {result:?}", path.display());
            assert!(result.is_ok(), "{}", path.display());
        }
    }
}
