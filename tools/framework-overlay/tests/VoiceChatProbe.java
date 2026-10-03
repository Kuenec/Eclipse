import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

public final class VoiceChatProbe {
    private static final int PERMISSION_GRANTED = 0;
    private static final int PERMISSION_DENIED = -1;
    private static final int AUDIOFOCUS_GAIN_TRANSIENT = 2;
    private static final int AUDIOFOCUS_REQUEST_GRANTED = 1;
    private static final int USAGE_VOICE_COMMUNICATION = 2;
    private static final int CONTENT_TYPE_SPEECH = 1;
    private static final String ROBLOX_PACKAGE = "com.roblox.client";

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static Object allocateWithoutLoadedApp(Class<?> type)
            throws ReflectiveOperationException {
        Class<?> unsafeClass = Class.forName("sun.misc.Unsafe");
        Object unsafe = unsafeClass.getMethod("getUnsafe").invoke(null);
        return unsafeClass.getMethod("allocateInstance", Class.class).invoke(unsafe, type);
    }

    private static void requirePermission(Object packageManager, Method checkPermission,
            String permission, int expected, String reason) throws ReflectiveOperationException {
        int actual = (Integer) checkPermission.invoke(packageManager, permission, ROBLOX_PACKAGE);
        require(actual == expected,
                permission + " returned " + actual + ", expected " + expected + ": " + reason);
    }

    private static void requireMicrophonePermission() throws ReflectiveOperationException {
        Class<?> packageManagerClass = Class.forName("android.content.pm.PackageManager");
        Object packageManager = allocateWithoutLoadedApp(packageManagerClass);
        Method checkPermission =
                packageManagerClass.getMethod("checkPermission", String.class, String.class);

        requirePermission(packageManager, checkPermission, "android.permission.RECORD_AUDIO",
                PERMISSION_GRANTED,
                "voice chat needs the microphone without ATL_UGLY_ENABLE_MICROPHONE");
        requirePermission(packageManager, checkPermission,
                "android.permission.MODIFY_AUDIO_SETTINGS", PERMISSION_GRANTED,
                "Android grants it to every app that declares it");
        requirePermission(packageManager, checkPermission,
                "android.permission.READ_EXTERNAL_STORAGE", PERMISSION_GRANTED,
                "ATL's storage grant still applies");
        requirePermission(packageManager, checkPermission,
                "android.permission.ACCESS_FINE_LOCATION", PERMISSION_DENIED,
                "ATL's location policy still applies");
    }

    private static Object voiceCallAudioAttributes() throws ReflectiveOperationException {
        Class<?> builderClass = Class.forName("android.media.AudioAttributes$Builder");
        Object builder = builderClass.getConstructor().newInstance();
        builderClass.getMethod("setUsage", int.class).invoke(builder, USAGE_VOICE_COMMUNICATION);
        builderClass.getMethod("setContentType", int.class).invoke(builder, CONTENT_TYPE_SPEECH);
        return builderClass.getMethod("build").invoke(builder);
    }

    private static Object focusChangeListener(Class<?> listenerClass) {
        InvocationHandler ignoreFocusChanges = new InvocationHandler() {
            @Override
            public Object invoke(Object proxy, Method method, Object[] arguments) {
                return null;
            }
        };
        return Proxy.newProxyInstance(listenerClass.getClassLoader(),
                new Class<?>[] {listenerClass}, ignoreFocusChanges);
    }

    private static void requireVoiceCallAudioManager() throws ReflectiveOperationException {
        Class<?> audioManagerClass = Class.forName("android.media.AudioManager");
        Object audioManager = audioManagerClass.getConstructor().newInstance();
        Class<?> attributesClass = Class.forName("android.media.AudioAttributes");
        Class<?> listenerClass =
                Class.forName("android.media.AudioManager$OnAudioFocusChangeListener");
        Class<?> requestClass = Class.forName("android.media.AudioFocusRequest");
        Class<?> builderClass = Class.forName("android.media.AudioFocusRequest$Builder");

        Object builder =
                builderClass.getConstructor(int.class).newInstance(AUDIOFOCUS_GAIN_TRANSIENT);
        builderClass.getMethod("setAudioAttributes", attributesClass)
                .invoke(builder, voiceCallAudioAttributes());
        builderClass.getMethod("setAcceptsDelayedFocusGain", boolean.class).invoke(builder, true);
        builderClass.getMethod("setWillPauseWhenDucked", boolean.class).invoke(builder, true);
        builderClass.getMethod("setOnAudioFocusChangeListener", listenerClass)
                .invoke(builder, focusChangeListener(listenerClass));
        Object request = builderClass.getMethod("build").invoke(builder);
        require(requestClass.isInstance(request), "build() returns an AudioFocusRequest");

        int requested = (Integer) audioManagerClass.getMethod("requestAudioFocus", requestClass)
                .invoke(audioManager, request);
        require(requested == AUDIOFOCUS_REQUEST_GRANTED,
                "requestAudioFocus(AudioFocusRequest) returned " + requested);
        int abandoned = (Integer) audioManagerClass
                .getMethod("abandonAudioFocusRequest", requestClass).invoke(audioManager, request);
        require(abandoned == AUDIOFOCUS_REQUEST_GRANTED,
                "abandonAudioFocusRequest(AudioFocusRequest) returned " + abandoned);
        require(!(Boolean) audioManagerClass.getMethod("isBluetoothScoAvailableOffCall")
                        .invoke(audioManager),
                "Bluetooth SCO is left to the host audio server");
        require(!(Boolean) audioManagerClass.getMethod("isVolumeFixed").invoke(audioManager),
                "the stream volume is not fixed");
    }

    public static void main(String[] arguments) throws ReflectiveOperationException {
        requireMicrophonePermission();
        requireVoiceCallAudioManager();
        System.out.println("voice-chat-ok");
        Runtime.getRuntime().halt(0);
    }
}
