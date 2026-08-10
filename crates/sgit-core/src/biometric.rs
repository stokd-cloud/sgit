//! Biometric human-presence gate for branch/repo locks
//! (AX-SGIT-LOCK-BIOMETRIC-GATE).
//!
//! [`require_biometric`] blocks until a physical biometric approval succeeds:
//!
//! * **macOS** — sign a nonce with an EPHEMERAL P-256 key generated inside the
//!   Secure Enclave under a biometry-gated access control
//!   (`kSecAccessControlBiometryCurrentSet` + `kSecAccessControlPrivateKeyUsage`).
//!   Every `SecKeyCreateSignature` against such a key raises Touch ID; the key
//!   is never persisted (`kSecAttrIsPermanent = false`), so no keychain
//!   entitlement is required — this is the proven unentitled-CLI path from the
//!   stokd governance-toggle signer, trimmed to a pure presence check.
//! * **Windows** — the WinRT `UserConsentVerifier` Windows Hello prompt,
//!   anchored to the console (or desktop) window, which MUST return `Verified`.
//! * **anything else** — `Err`: the gate FAILS CLOSED. There is no environment
//!   override and no non-biometric fallback; an agent can invoke the gate, but
//!   only a human at the sensor can clear it.

/// Raise the platform biometric prompt and require a positive verdict.
/// `reason` is surfaced to the terminal before the prompt and in errors.
pub fn require_biometric(reason: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        eprintln!("sgit: Touch ID required — {reason}");
        macos::require_biometric()
    }
    #[cfg(windows)]
    {
        eprintln!("sgit: Windows Hello required — {reason}");
        windows_hello::require_biometric(reason)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err(format!(
            "biometric approval is required ({reason}) but is only available on macOS \
             (Touch ID) or Windows (Windows Hello) — denying"
        ))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFMutableDictionary;
    use core_foundation::error::{CFError, CFErrorRef};
    use core_foundation::string::CFString;
    use security_framework::access_control::SecAccessControl;
    use security_framework::key::{Algorithm, SecKey};
    use security_framework_sys::access_control::{
        kSecAccessControlBiometryCurrentSet, kSecAccessControlPrivateKeyUsage,
    };
    use security_framework_sys::base::SecKeyRef;
    use security_framework_sys::item::{
        kSecAttrAccessControl, kSecAttrIsPermanent, kSecAttrKeySizeInBits, kSecAttrKeyType,
        kSecAttrKeyTypeECSECPrimeRandom, kSecAttrLabel, kSecAttrTokenID,
        kSecAttrTokenIDSecureEnclave, kSecPrivateKeyAttrs,
    };
    use security_framework_sys::key::SecKeyCreateRandomKey;

    /// Human-readable print-name of the ephemeral gate key (never persisted).
    const KEY_LABEL: &str = "sgit lock gate (Secure Enclave, ephemeral)";

    /// Create an ephemeral biometry-gated Secure Enclave P-256 key.
    fn ephemeral_biometry_key() -> Result<SecKey, String> {
        use core_foundation::base::ToVoid;
        use core_foundation::dictionary::CFDictionary;

        let flags = kSecAccessControlBiometryCurrentSet | kSecAccessControlPrivateKeyUsage;
        let access_control = SecAccessControl::create_with_flags(flags).map_err(|error| {
            format!("failed to build biometric access control (is Touch ID set up?): {error}")
        })?;

        let private_attrs: CFDictionary = CFMutableDictionary::from_CFType_pairs(&[
            (
                unsafe { kSecAttrIsPermanent }.to_void(),
                CFBoolean::false_value().to_void(),
            ),
            (
                unsafe { kSecAttrAccessControl }.to_void(),
                access_control.to_void(),
            ),
        ])
        .to_immutable();

        let key_size = core_foundation::number::CFNumber::from(256i32);
        let label = CFString::new(KEY_LABEL);
        let params: CFDictionary = CFMutableDictionary::from_CFType_pairs(&[
            (
                unsafe { kSecAttrKeyType }.to_void(),
                unsafe { kSecAttrKeyTypeECSECPrimeRandom }.to_void(),
            ),
            (
                unsafe { kSecAttrKeySizeInBits }.to_void(),
                key_size.to_void(),
            ),
            (
                unsafe { kSecAttrTokenID }.to_void(),
                unsafe { kSecAttrTokenIDSecureEnclave }.to_void(),
            ),
            (unsafe { kSecAttrLabel }.to_void(), label.to_void()),
            (
                unsafe { kSecPrivateKeyAttrs }.to_void(),
                private_attrs.to_void(),
            ),
        ])
        .to_immutable();

        let mut error: CFErrorRef = std::ptr::null_mut();
        let key_ref: SecKeyRef =
            unsafe { SecKeyCreateRandomKey(params.as_concrete_TypeRef(), &mut error) };
        if !error.is_null() {
            let err = unsafe { CFError::wrap_under_create_rule(error) };
            return Err(format!(
                "failed to create Secure Enclave gate key (no Touch ID hardware?): {err}"
            ));
        }
        if key_ref.is_null() {
            return Err("failed to create Secure Enclave gate key (null key)".to_string());
        }
        Ok(unsafe { SecKey::wrap_under_create_rule(key_ref) })
    }

    /// THE BIOMETRIC CALL: signing with the biometry-gated key raises Touch ID.
    /// A cancel/failure surfaces as an error — the gate stays closed.
    pub(super) fn require_biometric() -> Result<(), String> {
        let key = ephemeral_biometry_key()?;
        key.create_signature(
            Algorithm::ECDSASignatureMessageX962SHA256,
            b"sgit-lock-biometric-gate",
        )
        .map(|_| ())
        .map_err(|error| format!("biometric approval failed or was cancelled: {error}"))
    }
}

#[cfg(windows)]
mod windows_hello {
    use windows::core::{factory, HSTRING};
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Console::GetConsoleWindow;
    use windows::Win32::System::WinRT::IUserConsentVerifierInterop;
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
    use windows_future::IAsyncOperation;

    /// The console window (preferred anchor), or the desktop window when the
    /// process has no console.
    fn consent_anchor_window() -> HWND {
        let console = unsafe { GetConsoleWindow() };
        if console.0.is_null() {
            unsafe { GetDesktopWindow() }
        } else {
            console
        }
    }

    /// Raise the Windows Hello prompt and require a `Verified` result.
    pub(super) fn require_biometric(reason: &str) -> Result<(), String> {
        if let Ok(operation) = UserConsentVerifier::CheckAvailabilityAsync() {
            if let Ok(availability) = operation.join() {
                if availability != UserConsentVerifierAvailability::Available {
                    return Err(format!(
                        "Windows Hello is unavailable (availability {}) — denying. \
                         Set up Windows Hello (PIN + biometric) and retry.",
                        availability.0
                    ));
                }
            }
        }

        let message = HSTRING::from(format!("sgit lock: {reason}"));
        let anchor = consent_anchor_window();
        let interop: IUserConsentVerifierInterop =
            factory::<UserConsentVerifier, IUserConsentVerifierInterop>()
                .map_err(|error| format!("Windows Hello interop unavailable: {error}"))?;
        let operation: IAsyncOperation<UserConsentVerificationResult> =
            unsafe { interop.RequestVerificationForWindowAsync(anchor, &message) }
                .map_err(|error| format!("Windows Hello prompt failed to start: {error}"))?;
        let result = operation
            .join()
            .map_err(|error| format!("Windows Hello verification failed: {error}"))?;
        if result == UserConsentVerificationResult::Verified {
            Ok(())
        } else {
            Err(format!(
                "Windows Hello was not confirmed (result {}) — denying",
                result.0
            ))
        }
    }
}
