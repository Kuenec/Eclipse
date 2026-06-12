/*
 * 2026-06-11: ECLIPSE PATCH — derived from ATL's api-impl `android.os.PowerManager`
 * (Apache-2.0). Patch adds `isDeviceIdleMode()` (marked below); everything else is a
 * verbatim copy so the shadowing class keeps ATL's full member surface.
 *
 * WHY: Roblox's jobqueue library (com.birbit.android.jobqueue) calls
 * `isDeviceIdleMode()` from its job-manager worker thread; ATL omits the method, and the
 * resulting uncaught NoSuchMethodError is PROCESS-FATAL — ATL's vendored libcore installs
 * a default uncaught-exception handler that calls System.exit(10) on any thread's
 * uncaught exception (Thread.java `hacky_uncaught_exception_handler`, mirroring AOSP's
 * KillApplicationHandler).
 */
package android.os;

public final class PowerManager {
	public final class WakeLock {
		public void setReferenceCounted(boolean referenceCounted) {}

		public void acquire() {}

		public void release() {}

		public boolean isHeld() {
			return false;
		}

		public void acquire(long timeout) {}
	}

	public WakeLock newWakeLock(int levelAndFlags, String tag) {
		return new WakeLock();
	}

	public void userActivity(long dummy, boolean dummy2) {}

	public static final int FULL_WAKE_LOCK = 0x1a;

	public boolean isPowerSaveMode() {
		return false;
	}

	// ECLIPSE PATCH 2026-06-11: AOSP API 23+ Doze query; a desktop host is never in Doze.
	public boolean isDeviceIdleMode() {
		return false;
	}

	public boolean isScreenOn() {
		return true;
	}

	public boolean isIgnoringBatteryOptimizations(String packageName) {
		return true;
	}
}
