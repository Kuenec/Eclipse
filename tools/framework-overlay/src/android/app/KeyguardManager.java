/*
 * 2026-07-01: ECLIPSE PATCH — derived from ATL's api-impl `android.app.KeyguardManager`
 * (Apache-2.0). Patch adds `isDeviceSecure()` (marked below); everything else is a
 * verbatim copy of the installed class so the shadowing class keeps ATL's full member surface.
 *
 * WHY: Roblox's security/device-check path (com.roblox.universalapp.messagebus MessageBus
 * handler → ak.t.a(Activity) → ak.n.a(Context)) calls `KeyguardManager.isDeviceSecure()`
 * (AOSP API 23+, the modern replacement for the deprecated isKeyguardSecure()); ATL's
 * KeyguardManager omits it. The resulting NoSuchMethodError propagates out of a JNI
 * transition as a pending exception, tripping ART's `runtime.cc:650 No pending exception
 * expected` FATAL abort (SIGABRT, EXIT=134) — i.e. it is PROCESS-FATAL, killing the boot at
 * the account/login screen.
 */
package android.app;

public class KeyguardManager {
	public boolean inKeyguardRestrictedInputMode() {
		return false;
	}

	public boolean isKeyguardLocked() {
		return false;
	}

	public boolean isKeyguardSecure() {
		return true;
	}

	// ECLIPSE PATCH 2026-07-01: AOSP API 23+ KeyguardManager.isDeviceSecure() — "whether the
	// device is secured with a PIN, pattern or password". Eclipse's host has NO secure lock
	// screen, so the honest answer is false; this also keeps Roblox from attempting a
	// device-credential / confirm-credential flow (KeyguardManager.createConfirmDeviceCredentialIntent)
	// that a headless host cannot satisfy. (isKeyguardSecure() above stays true, matching ATL's
	// installed class verbatim — it has SIM-lock semantics and Eclipse does not touch it.)
	public boolean isDeviceSecure() {
		return false;
	}
}
