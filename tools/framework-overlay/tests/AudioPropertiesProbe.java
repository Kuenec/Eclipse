import java.lang.reflect.Method;

public final class AudioPropertiesProbe {
    private static void requireProperty(Object audioManager, Method getProperty, String property,
            String expected, String reason) throws ReflectiveOperationException {
        Object actual = getProperty.invoke(audioManager, property);
        if (!expected.equals(actual)) {
            throw new AssertionError(property + " returned " + actual + ", expected " + expected
                    + ": " + reason);
        }
    }

    public static void main(String[] arguments) throws ReflectiveOperationException {
        Class<?> audioManagerClass = Class.forName("android.media.AudioManager");
        Object audioManager = audioManagerClass.getConstructor().newInstance();
        Method getProperty = audioManagerClass.getMethod("getProperty", String.class);

        requireProperty(audioManager, getProperty, "android.media.property.OUTPUT_SAMPLE_RATE",
                "48000", "FMOD must see the rate Eclipse publishes for AAudio, not ATL's 44100");
        requireProperty(audioManager, getProperty,
                "android.media.property.OUTPUT_FRAMES_PER_BUFFER", "512",
                "FMOD must see the low-latency AAudio burst Eclipse publishes, not ATL's 256");

        System.out.println("audio-properties-ok");
        Runtime.getRuntime().halt(0);
    }
}
