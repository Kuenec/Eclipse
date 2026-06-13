/*
 * 2026-06-11: ECLIPSE PATCH — derived from ATL's api-impl `android.app.ActivityManager`
 * (Apache-2.0). Patch is confined to `RunningAppProcessInfo` (marked below); everything
 * else is a verbatim copy so the shadowing class keeps ATL's full member surface.
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

	public boolean isLowRamDevice() {return false;}

	public static class MemoryInfo implements android.os.Parcelable {
		/* For now, just always report there's 10GB free RAM */
		public long availMem = 10000;

		public long totalMem = 10000;

		public long threshold = 200;

		public boolean lowMemory = false;

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
		}
	}

	public void getMemoryInfo(MemoryInfo outInfo)
	{
		outInfo = new MemoryInfo();
	}

	public ConfigurationInfo getDeviceConfigurationInfo() {
		return new ConfigurationInfo();
	}

	public int getMemoryClass() {return 20;}  // suggested heap size in MB
	public int getLargeMemoryClass() {return 60;} // value chosen arbitrarily

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
