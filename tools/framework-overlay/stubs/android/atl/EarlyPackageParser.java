package android.atl;

import java.io.File;

public final class EarlyPackageParser {
  private EarlyPackageParser() {}

  public static int parseMinSdkInt(File apk, int def) {
    return def;
  }
}
