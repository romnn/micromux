use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::ControlServer;
use crate::ControlError;
use crate::endpoint::ControlEndpoint;

/// Placeholder endpoint guard for platforms where the control transport is not implemented.
pub struct EndpointGuard;

pub(super) fn bind(_endpoint: &ControlEndpoint) -> Result<Option<EndpointGuard>, ControlError> {
    Err(ControlError::Unsupported)
}

pub(super) async fn serve(
    _server: Arc<ControlServer>,
    _guard: EndpointGuard,
    _shutdown: CancellationToken,
) -> Result<(), ControlError> {
    Err(ControlError::Unsupported)
}
