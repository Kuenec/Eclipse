/*
 * 2026-06-11: ECLIPSE PATCH — derived from ATL's api-impl `android.app.ActivityManager`
 * (Apache-2.0). Eclipse patches `RunningAppProcessInfo` and the host-backed memory surface
 * (each marked below); everything else follows ATL so the shadowing class keeps its full surface.
 *
 * WHY: Roblox decides main-vs-background process (dex method `yj.s.b`, v2.721.1108) by
 * scanning `getRunningAppProcesses()` for an entry with `importance ==
 * IMPORTANCE_FOREGROUND (100)` whose `pkgList` contains the package name. ATL's
 * `RunningAppProcessInfo` left `importance` at 0 and had NO `pkgList` field, so the scan
 * matched nothing and Roblox logged "Background process detected".
 */
package android.app;

import android.content.Context;
import android.content.pm.ConfigurationInfo;
import android.graphics.Bitmap;
import android.os.Bundle;
import android.os.IBinder;
import android.os.Parcel;
import android.os.Parcelable;
import android.os.Process;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Collections;

public class ActivityManager {

	public static class RunningAppProcessInfo{
		// ECLIPSE PATCH 2026-06-11: AOSP-standard members ATL omits. Eclipse always runs
		// the app as the single foreground process, so the one reported entry IS
		// IMPORTANCE_FOREGROUND with the package in pkgList (AOSP value: 100).
		public static final int IMPORTANCE_FOREGROUND = 100;

		public int importance;
		public int pid;
		public int uid;
		public String processName;
		// ECLIPSE PATCH 2026-06-11: AOSP field (packages running in this process).
		public String[] pkgList;

		// ECLIPSE PATCH 2026-06-11: AOSP has a public no-arg constructor (used with
		// getMyMemoryState by apps).
		public RunningAppProcessInfo() {}

		private RunningAppProcessInfo(int pid, String processName) {
			this.pid = pid;
			this.processName = processName;
			// ECLIPSE PATCH 2026-06-11: see class comment.
			this.importance = IMPORTANCE_FOREGROUND;
			this.pkgList = new String[] {processName};
		}
	}

	public static class TaskDescription {
		public TaskDescription(String name) {}
		public TaskDescription(String name, Bitmap icon, int color) {}
	}

	public List<RunningAppProcessInfo> getRunningAppProcesses() {
		return Arrays.asList(new RunningAppProcessInfo(Process.myPid(), Context.this_application.getPackageName()));
	}

	/*
	 * ECLIPSE PATCH 2026-07-18: ATL hardcoded false and reported 10,000 bytes of RAM.
	 * Eclipse's Rust backing reads the Linux kernel and applies the runtime's low-RAM policy.
	 */
	private static native boolean native_isLowRamDevice();
	public boolean isLowRamDevice() {return native_isLowRamDevice();}

	public static class MemoryInfo implements android.os.Parcelable {
		// ECLIPSE PATCH 2026-07-18: zero until getMemoryInfo fills this object from the kernel.
		public long availMem;

		public long totalMem;

		public long threshold;

		public boolean lowMemory;

		/*
		 * ECLIPSE PATCH 2026-07-18: AOSP's hidden low-memory-killer fields are part of the
		 * Parcelable wire shape. Current Roblox checks for these final 32 bytes and reads the
		 * background/foreground thresholds into DeviceParams. Linux has no Android LMK, so the
		 * Rust fill leaves all four explicitly zero instead of inventing a phone profile.
		 */
		public long hiddenAppThreshold;
		public long secondaryServerThreshold;
		public long visibleAppThreshold;
		public long foregroundAppThreshold;

		// 2026-06-13: ECLIPSE PATCH — AOSP MemoryInfo is Parcelable; Roblox calls writeToParcel in
		// ActivityNativeMain.onResume startup (dex tg.b2.b). Write the fields via the stock Parcel
		// write-API (verified present on ATL's installed Parcel: writeLong/writeInt). No FDs -> 0.
		public int describeContents() {
			return 0;
		}

		public void writeToParcel(android.os.Parcel dest, int flags) {
			dest.writeLong(availMem);
			dest.writeLong(totalMem);
			dest.writeLong(threshold);
			dest.writeInt(lowMemory ? 1 : 0);
			dest.writeLong(hiddenAppThreshold);
			dest.writeLong(secondaryServerThreshold);
			dest.writeLong(visibleAppThreshold);
			dest.writeLong(foregroundAppThreshold);
		}
	}

	/*
	 * ECLIPSE PATCH 2026-07-18: getMemoryInfo is an out-parameter API. ATL assigned a new object
	 * only to its local parameter, so every caller retained the bogus initializer and Roblox built
	 * an impossible `0MB` device profile. Rust owns the host query and mutates the supplied object.
	 */
	private static native void native_fillMemoryInfo(MemoryInfo outInfo);
	public void getMemoryInfo(MemoryInfo outInfo)
	{
		if (outInfo == null) {
			throw new NullPointerException("outInfo");
		}
		native_fillMemoryInfo(outInfo);
	}

	public ConfigurationInfo getDeviceConfigurationInfo() {
		return new ConfigurationInfo();
	}

	// ECLIPSE PATCH 2026-07-18: report the actual -Xmx/HeapGrowthLimit Eclipse gives ART.
	private static native int native_getMemoryClass();
	private static native int native_getLargeMemoryClass();
	public int getMemoryClass() {return native_getMemoryClass();}
	public int getLargeMemoryClass() {return native_getLargeMemoryClass();}

	public static void getMyMemoryState(RunningAppProcessInfo outInfo) {}

	public boolean clearApplicationUserData() {return false;}

	public static class AppTask {}
	public List<ActivityManager.AppTask> getAppTasks() {
		return new ArrayList<>();
	}

	public static class RunningServiceInfo implements Parcelable {
		public RunningServiceInfo() {
		}

		public int describeContents() {
			return 0;
		}

		public void writeToParcel(Parcel dest, int flags) {
			return;
		}

		public void readFromParcel(Parcel source) {
			return;
		}
	}


	public List<RunningServiceInfo> getRunningServices(int maxNum)
		throws SecurityException {
			return new ArrayList<>();
	}

	public List<ApplicationExitInfo> getHistoricalProcessExitReasons(String pkgname, int pid, int maxNum) {
		return Collections.emptyList();
	}

	public static boolean isUserAMonkey() {return false;}

	public void moveTaskToFront(int taskId, int flags, Bundle options) {
	}
}
