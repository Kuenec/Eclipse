package android.webkit;

public class WebResourceError {
  private final int errorCode;

  private final CharSequence description;

  WebResourceError(int errorCode, CharSequence description) {
    this.errorCode = errorCode;
    this.description = description;
  }

  public int getErrorCode() {
    return errorCode;
  }

  public CharSequence getDescription() {
    return description;
  }
}
