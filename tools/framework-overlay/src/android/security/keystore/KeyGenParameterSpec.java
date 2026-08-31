package android.security.keystore;

import java.math.BigInteger;
import java.security.spec.AlgorithmParameterSpec;
import java.util.Date;
import java.util.Objects;
import javax.security.auth.x500.X500Principal;

public final class KeyGenParameterSpec implements AlgorithmParameterSpec {
  private final String keystoreAlias;
  private final int purposes;
  private final int keySize;
  private final AlgorithmParameterSpec algorithmParameterSpec;
  private final String[] blockModes;
  private final String[] encryptionPaddings;
  private final String[] digests;
  private final byte[] attestationChallenge;
  private final Date keyValidityStart;
  private final boolean strongBoxBacked;
  private final boolean userAuthenticationRequired;
  private final X500Principal certificateSubject;
  private final BigInteger certificateSerialNumber;
  private final Date certificateNotBefore;
  private final Date certificateNotAfter;

  private KeyGenParameterSpec(Builder builder) {
    keystoreAlias = builder.keystoreAlias;
    purposes = builder.purposes;
    keySize = builder.keySize;
    algorithmParameterSpec = builder.algorithmParameterSpec;
    blockModes = cloneStrings(builder.blockModes);
    encryptionPaddings = cloneStrings(builder.encryptionPaddings);
    digests = cloneStrings(builder.digests);
    attestationChallenge = cloneBytes(builder.attestationChallenge);
    keyValidityStart = cloneDate(builder.keyValidityStart);
    strongBoxBacked = builder.strongBoxBacked;
    userAuthenticationRequired = builder.userAuthenticationRequired;
    certificateSubject = builder.certificateSubject;
    certificateSerialNumber = builder.certificateSerialNumber;
    certificateNotBefore = cloneDate(builder.certificateNotBefore);
    certificateNotAfter = cloneDate(builder.certificateNotAfter);
  }

  private static String[] cloneStrings(String[] values) {
    return values == null ? null : values.clone();
  }

  private static byte[] cloneBytes(byte[] values) {
    return values == null ? null : values.clone();
  }

  private static Date cloneDate(Date value) {
    return value == null ? null : new Date(value.getTime());
  }

  public String getKeystoreAlias() {
    return keystoreAlias;
  }

  public int getPurposes() {
    return purposes;
  }

  public int getKeySize() {
    return keySize;
  }

  public AlgorithmParameterSpec getAlgorithmParameterSpec() {
    return algorithmParameterSpec;
  }

  public String[] getBlockModes() {
    return cloneStrings(blockModes);
  }

  public String[] getEncryptionPaddings() {
    return cloneStrings(encryptionPaddings);
  }

  public boolean isDigestsSpecified() {
    return digests != null;
  }

  public String[] getDigests() {
    if (digests == null) {
      throw new IllegalStateException("Digests not specified");
    }
    return digests.clone();
  }

  public byte[] getAttestationChallenge() {
    return cloneBytes(attestationChallenge);
  }

  public Date getKeyValidityStart() {
    return cloneDate(keyValidityStart);
  }

  public boolean isStrongBoxBacked() {
    return strongBoxBacked;
  }

  public boolean isUserAuthenticationRequired() {
    return userAuthenticationRequired;
  }

  public X500Principal getCertificateSubject() {
    return certificateSubject;
  }

  public BigInteger getCertificateSerialNumber() {
    return certificateSerialNumber;
  }

  public Date getCertificateNotBefore() {
    return cloneDate(certificateNotBefore);
  }

  public Date getCertificateNotAfter() {
    return cloneDate(certificateNotAfter);
  }

  public static final class Builder {
    private final String keystoreAlias;
    private final int purposes;
    private int keySize = -1;
    private AlgorithmParameterSpec algorithmParameterSpec;
    private String[] blockModes;
    private String[] encryptionPaddings;
    private String[] digests;
    private byte[] attestationChallenge;
    private Date keyValidityStart;
    private boolean strongBoxBacked;
    private boolean userAuthenticationRequired;
    private X500Principal certificateSubject = new X500Principal("CN=Fake");
    private BigInteger certificateSerialNumber = BigInteger.ONE;
    private Date certificateNotBefore = new Date(0L);
    private Date certificateNotAfter = new Date(2461449600000L);

    public Builder(String keystoreAlias, int purposes) {
      this.keystoreAlias = Objects.requireNonNull(keystoreAlias, "keystoreAlias == null");
      if (keystoreAlias.isEmpty()) {
        throw new IllegalArgumentException("keystoreAlias must not be empty");
      }
      this.purposes = purposes;
    }

    public Builder setKeySize(int keySize) {
      if (keySize < 0) {
        throw new IllegalArgumentException("keySize < 0");
      }
      this.keySize = keySize;
      return this;
    }

    public Builder setAlgorithmParameterSpec(AlgorithmParameterSpec spec) {
      algorithmParameterSpec = Objects.requireNonNull(spec, "spec == null");
      return this;
    }

    public Builder setBlockModes(String... blockModes) {
      this.blockModes = cloneStrings(Objects.requireNonNull(blockModes, "blockModes == null"));
      return this;
    }

    public Builder setEncryptionPaddings(String... encryptionPaddings) {
      this.encryptionPaddings =
          cloneStrings(Objects.requireNonNull(encryptionPaddings, "encryptionPaddings == null"));
      return this;
    }

    public Builder setDigests(String... digests) {
      this.digests = cloneStrings(Objects.requireNonNull(digests, "digests == null"));
      return this;
    }

    public Builder setAttestationChallenge(byte[] attestationChallenge) {
      this.attestationChallenge = cloneBytes(attestationChallenge);
      return this;
    }

    public Builder setKeyValidityStart(Date keyValidityStart) {
      this.keyValidityStart = cloneDate(keyValidityStart);
      return this;
    }

    public Builder setIsStrongBoxBacked(boolean strongBoxBacked) {
      this.strongBoxBacked = strongBoxBacked;
      return this;
    }

    public Builder setUserAuthenticationRequired(boolean userAuthenticationRequired) {
      this.userAuthenticationRequired = userAuthenticationRequired;
      return this;
    }

    public Builder setCertificateSubject(X500Principal certificateSubject) {
      this.certificateSubject =
          Objects.requireNonNull(certificateSubject, "certificateSubject == null");
      return this;
    }

    public Builder setCertificateSerialNumber(BigInteger certificateSerialNumber) {
      this.certificateSerialNumber =
          Objects.requireNonNull(certificateSerialNumber, "certificateSerialNumber == null");
      return this;
    }

    public Builder setCertificateNotBefore(Date certificateNotBefore) {
      this.certificateNotBefore =
          cloneDate(Objects.requireNonNull(certificateNotBefore, "certificateNotBefore == null"));
      return this;
    }

    public Builder setCertificateNotAfter(Date certificateNotAfter) {
      this.certificateNotAfter =
          cloneDate(Objects.requireNonNull(certificateNotAfter, "certificateNotAfter == null"));
      return this;
    }

    public KeyGenParameterSpec build() {
      return new KeyGenParameterSpec(this);
    }
  }
}
