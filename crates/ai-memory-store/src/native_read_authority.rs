//! Live request authority for native source inspection.
use std::{future::Future, pin::Pin};

use crate::SourceAuthorization;

/// Recheck the actual authenticating origin before capture and before release.
/// Implementations retain private request proofs and their authenticating service.
/// Absence, changed identity, inactive credentials and backend failures return
/// `None`; callers must refuse without exposing authentication diagnostics.
pub trait NativeReadAuthority: Send + Sync {
    /// Current source authority, with no serialized credential material.
    fn recheck(&self) -> Pin<Box<dyn Future<Output = Option<SourceAuthorization>> + Send + '_>>;
}
