package android.atl;

import android.app.Application;

public final class ATLLoadedApp {
  private ATLLoadedApp() {}

  public static ATLLoadedApp getPrimaryApplication() {
    return null;
  }

  public Application getApplication() {
    return null;
  }
}
