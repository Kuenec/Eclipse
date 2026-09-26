package android.content.pm;

public final class SigningInfo {
  private final Signature[] signatures;
  private final Signature[] pastSigningCertificates;

  public SigningInfo() {
    this(null, null);
  }

  SigningInfo(Signature[] signatures, Signature[] pastSigningCertificates) {
    this.signatures = signatures;
    this.pastSigningCertificates = pastSigningCertificates;
  }

  public boolean hasMultipleSigners() {
    return signatures != null && signatures.length > 1;
  }

  public boolean hasPastSigningCertificates() {
    return pastSigningCertificates != null && pastSigningCertificates.length > 0;
  }

  public Signature[] getSigningCertificateHistory() {
    if (hasMultipleSigners()) {
      return null;
    }
    if (!hasPastSigningCertificates()) {
      return signatures;
    }
    return pastSigningCertificates;
  }

  public Signature[] getApkContentsSigners() {
    return signatures;
  }
}
