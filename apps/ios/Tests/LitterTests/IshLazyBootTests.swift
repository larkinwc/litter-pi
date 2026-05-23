import XCTest
@testable import Litter

/// Verifies that the iSH Alpine kernel is NOT booted as part of host-app
/// launch. The kernel is deferred to the first tool-exec invocation via
/// `ish_runtime::ensure_booted` in `codex-mobile-client`.
///
/// Background: the upstream `dnakov/litter-ish` kernel aborts with
/// `invalid vdso. this should never happen.` on iOS 26.x simulator
/// runtimes, which would crash the host process before XCTest can attach.
/// Lazy boot lets every non-pi XCTest survive long enough for XCTest to
/// run on those simulators, and improves production cold-start because
/// the rootfs extraction + kernel boot no longer block app launch.
///
/// See `library/litter-ish-compatibility.md` for the upstream bug.
@MainActor
final class IshLazyBootTests: XCTestCase {
    /// The host app must survive long enough for an XCTest case to even
    /// execute. That alone is the strongest evidence that the iSH kernel
    /// did not abort the process during launch — historically the `invalid
    /// vdso` abort fired between `application(_:didFinishLaunching…)` and
    /// the first XCTest method. We also assert `ishIsKernelBooted()` is
    /// false here so the test fails loudly if a future change reintroduces
    /// eager boot on launch (rather than silently regressing cold-start).
    func testKernelNotBootedDuringHostAppLaunch() {
        XCTAssertFalse(
            ishIsKernelBooted(),
            "iSH kernel must remain unbooted at host-app launch; the lazy boot path is responsible for faulting it in on the first tool-exec call. See library/litter-ish-compatibility.md."
        )
    }
}
