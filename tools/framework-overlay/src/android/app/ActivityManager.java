
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
import java.util.Collections;
import java.util.List;

public class ActivityManager {
  public static class RunningAppProcessInfo {
    public static final int IMPORTANCE_FOREGROUND = 100;

    public int importance;
    public int pid;
    public int uid;
    public String processName;

    public String[] pkgList;

    public RunningAppProcessInfo() {}

    private RunningAppProcessInfo(int pid, String processName) {
      this.pid = pid;
      this.processName = processName;

      this.importance = IMPORTANCE_FOREGROUND;
      this.pkgList = new String[] {processName};
    }
  }

  public static class TaskDescription {
    public TaskDescription(String name) {}
    public TaskDescription(String name, Bitmap icon, int color) {}
  }

  public List<RunningAppProcessInfo> getRunningAppProcesses() {
    return Arrays.asList(
        new RunningAppProcessInfo(Process.myPid(), Context.this_application.getPackageName()));
  }

  private static native boolean native_isLowRamDevice();
  public boolean isLowRamDevice() {
    return native_isLowRamDevice();
  }

  public static class MemoryInfo implements android.os.Parcelable {
    public long availMem;

    public long totalMem;

    public long threshold;

    public boolean lowMemory;

    public long hiddenAppThreshold;
    public long secondaryServerThreshold;
    public long visibleAppThreshold;
    public long foregroundAppThreshold;

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

  private static native void native_fillMemoryInfo(MemoryInfo outInfo);
  public void getMemoryInfo(MemoryInfo outInfo) {
    if (outInfo == null) {
      throw new NullPointerException("outInfo");
    }
    native_fillMemoryInfo(outInfo);
  }

  public ConfigurationInfo getDeviceConfigurationInfo() {
    return new ConfigurationInfo();
  }

  private static native int native_getMemoryClass();
  private static native int native_getLargeMemoryClass();
  public int getMemoryClass() {
    return native_getMemoryClass();
  }
  public int getLargeMemoryClass() {
    return native_getLargeMemoryClass();
  }

  public static void getMyMemoryState(RunningAppProcessInfo outInfo) {}

  public boolean clearApplicationUserData() {
    return false;
  }

  public static class AppTask {}
  public List<ActivityManager.AppTask> getAppTasks() {
    return new ArrayList<>();
  }

  public static class RunningServiceInfo implements Parcelable {
    public RunningServiceInfo() {}

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

  public List<RunningServiceInfo> getRunningServices(int maxNum) throws SecurityException {
    return new ArrayList<>();
  }

  public List<ApplicationExitInfo> getHistoricalProcessExitReasons(
      String pkgname, int pid, int maxNum) {
    return Collections.emptyList();
  }

  public static boolean isUserAMonkey() {
    return false;
  }

  public void moveTaskToFront(int taskId, int flags, Bundle options) {}
}
