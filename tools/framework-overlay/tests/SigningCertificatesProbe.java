package android.content.pm;

public final class SigningCertificatesProbe {
  private static final int GET_SIGNATURES_AND_SIGNING_CERTIFICATES =
      PackageManager.GET_SIGNATURES | PackageManager.GET_SIGNING_CERTIFICATES;

  private static void require(boolean condition, String message) {
    if (!condition) {
      throw new AssertionError(message);
    }
  }

  private static PackageInfo fill(PackageParser.Package pkg, int flags) {
    PackageInfo info = new PackageInfo();
    SigningCertificates.fillPackageInfo(pkg, flags, info);
    return info;
  }

  public static void main(String[] arguments) {
    Signature original = new Signature(new byte[] {1});
    Signature rotated = new Signature(new byte[] {2});

    PackageParser.Package rotatedPackage = new PackageParser.Package();
    rotatedPackage.mSignatures = new Signature[] {rotated};
    rotatedPackage.mPastSigningCertificates = new Signature[] {original, rotated};

    PackageInfo info = fill(rotatedPackage, GET_SIGNATURES_AND_SIGNING_CERTIFICATES);
    require(info.signatures.length == 1, "GET_SIGNATURES must report one certificate");
    require(info.signatures[0] == original, "GET_SIGNATURES must report the oldest certificate");
    require(info.signingInfo.getApkContentsSigners().length == 1, "one current signer");
    require(info.signingInfo.getApkContentsSigners()[0] == rotated, "current signer is rotated");
    require(info.signingInfo.hasPastSigningCertificates(), "rotation history is reported");
    require(!info.signingInfo.hasMultipleSigners(), "a rotated package has one signer");
    Signature[] history = info.signingInfo.getSigningCertificateHistory();
    require(history.length == 2, "history holds both certificates");
    require(history[0] == original && history[1] == rotated, "history runs oldest first");

    info.signingInfo.getApkContentsSigners()[0] = original;
    history[0] = rotated;
    require(rotatedPackage.mSignatures[0] == rotated, "PackageInfo aliased the signer array");
    require(
        rotatedPackage.mPastSigningCertificates[0] == original,
        "PackageInfo aliased the rotation history");

    PackageParser.Package singlePackage = new PackageParser.Package();
    singlePackage.mSignatures = new Signature[] {original};
    info = fill(singlePackage, GET_SIGNATURES_AND_SIGNING_CERTIFICATES);
    require(info.signatures.length == 1 && info.signatures[0] == original, "single signer");
    require(info.signatures != singlePackage.mSignatures, "GET_SIGNATURES aliased the signers");
    require(!info.signingInfo.hasPastSigningCertificates(), "no rotation history");
    require(
        info.signingInfo.getSigningCertificateHistory()[0] == original,
        "without rotation the history is the signer");

    SigningInfo untouched = new SigningInfo();
    info = new PackageInfo();
    info.signingInfo = untouched;
    SigningCertificates.fillPackageInfo(rotatedPackage, PackageManager.GET_SIGNATURES, info);
    require(info.signingInfo == untouched, "GET_SIGNATURES alone must not set signingInfo");
    info = fill(rotatedPackage, PackageManager.GET_SIGNING_CERTIFICATES);
    require(info.signatures == null, "GET_SIGNING_CERTIFICATES alone must not set signatures");

    info = fill(new PackageParser.Package(), GET_SIGNATURES_AND_SIGNING_CERTIFICATES);
    require(info.signatures == null, "an unsigned package reports no signatures");
    require(info.signingInfo == null, "an unsigned package reports no signing info");

    require(untouched.getApkContentsSigners() == null, "empty SigningInfo has no signers");
    require(untouched.getSigningCertificateHistory() == null, "empty SigningInfo has no history");

    System.out.println("signing-certificates-ok");
  }
}
