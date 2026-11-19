package android.util;

import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

public final class EclipseMissingApi {
  private static final String TAG = "EclipseMissingApi";
  private static final int REPORTED_LIMIT = 256;
  private static final Set<String> REPORTED = ConcurrentHashMap.newKeySet();

  private EclipseMissingApi() {}

  public static void used(String member) {
    if (REPORTED.size() < REPORTED_LIMIT && REPORTED.add(member)) {
      Slog.w(TAG, member);
    }
  }
}
