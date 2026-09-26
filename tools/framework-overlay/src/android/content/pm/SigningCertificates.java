package android.content.pm;

public final class SigningCertificates {
  private SigningCertificates() {}

  private static native byte[][] native_signingCertificateHistory();

  public static void collectHostVerified(PackageParser.Package pkg) {
    byte[][] encoded = native_signingCertificateHistory();
    Signature[] history = new Signature[encoded.length];
    for (int i = 0; i < encoded.length; i++) {
      history[i] = new Signature(encoded[i]);
    }
    pkg.mSignatures = new Signature[] {history[history.length - 1]};
    pkg.mPastSigningCertificates = history.length > 1 ? history : null;
  }

  static void fillPackageInfo(PackageParser.Package pkg, int flags, PackageInfo info) {
    Signature[] signatures = pkg.mSignatures;
    Signature[] past = pkg.mPastSigningCertificates;
    if ((flags & PackageManager.GET_SIGNATURES) != 0) {
      if (past != null) {
        info.signatures = new Signature[] {past[0]};
      } else if (signatures != null && signatures.length > 0) {
        info.signatures = signatures.clone();
      }
    }
    if ((flags & PackageManager.GET_SIGNING_CERTIFICATES) != 0) {
      info.signingInfo =
          signatures == null
              ? null
              : new SigningInfo(signatures.clone(), past == null ? null : past.clone());
    }
  }
}
