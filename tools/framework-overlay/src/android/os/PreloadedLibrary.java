package android.os;

public final class PreloadedLibrary {
  private PreloadedLibrary() {}

  public static native String initialize(String name);
}
