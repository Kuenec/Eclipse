import android.security.keystore.KeyGenParameterSpec;
import java.math.BigInteger;
import java.security.spec.ECGenParameterSpec;
import java.util.Date;
import javax.security.auth.x500.X500Principal;

public final class KeyGenParameterSpecProbe {
  private static void require(boolean condition, String message) {
    if (!condition) {
      throw new AssertionError(message);
    }
  }

  public static void main(String[] arguments) {
    String[] digests = {"SHA-256"};
    byte[] challenge = {1, 2, 3};
    Date validityStart = new Date(1234L);
    Date certificateNotBefore = new Date(2345L);
    Date certificateNotAfter = new Date(3456L);
    ECGenParameterSpec algorithm = new ECGenParameterSpec("secp256r1");
    X500Principal subject = new X500Principal("CN=Probe");

    KeyGenParameterSpec spec =
        new KeyGenParameterSpec.Builder("probe", 12)
            .setAlgorithmParameterSpec(algorithm)
            .setDigests(digests)
            .setAttestationChallenge(challenge)
            .setKeyValidityStart(validityStart)
            .setIsStrongBoxBacked(true)
            .setCertificateSubject(subject)
            .setCertificateSerialNumber(BigInteger.TEN)
            .setCertificateNotBefore(certificateNotBefore)
            .setCertificateNotAfter(certificateNotAfter)
            .build();

    digests[0] = "changed";
    challenge[0] = 9;
    validityStart.setTime(0L);
    certificateNotBefore.setTime(0L);
    certificateNotAfter.setTime(0L);

    require(spec.getAlgorithmParameterSpec() == algorithm, "algorithm parameters were not retained");
    require("SHA-256".equals(spec.getDigests()[0]), "digest input was not copied");
    require(spec.getAttestationChallenge()[0] == 1, "challenge input was not copied");
    require(spec.getKeyValidityStart().getTime() == 1234L, "validity input was not copied");
    require(spec.isStrongBoxBacked(), "StrongBox choice was not retained");
    require(subject.equals(spec.getCertificateSubject()), "certificate subject was not retained");
    require(BigInteger.TEN.equals(spec.getCertificateSerialNumber()), "serial was not retained");
    require(spec.getCertificateNotBefore().getTime() == 2345L, "not-before was not copied");
    require(spec.getCertificateNotAfter().getTime() == 3456L, "not-after was not copied");

    byte[] returnedChallenge = spec.getAttestationChallenge();
    returnedChallenge[0] = 8;
    require(spec.getAttestationChallenge()[0] == 1, "challenge output was not copied");

    boolean nullRejected = false;
    try {
      new KeyGenParameterSpec.Builder("probe", 12).setAlgorithmParameterSpec(null);
    } catch (NullPointerException expected) {
      nullRejected = true;
    }
    require(nullRejected, "null algorithm parameters were accepted");

    System.out.print("keygen-parameter-spec-ok");
  }
}
