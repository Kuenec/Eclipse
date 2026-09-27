import java.lang.reflect.Method;
import java.util.Arrays;

public final class DisplayRefreshRateProbe {
    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    public static void main(String[] arguments) throws ReflectiveOperationException {
        Class<?> displayClass = Class.forName("android.view.Display");
        Method refreshRate = displayClass.getMethod("getRefreshRate");
        Method supportedRefreshRates = displayClass.getMethod("getSupportedRefreshRates");
        Method currentMode = displayClass.getMethod("getMode");
        Method supportedModes = displayClass.getMethod("getSupportedModes");
        Method setRefreshRates =
                displayClass.getMethod("setRefreshRates", float.class, float[].class);
        Method modeRefreshRate = currentMode.getReturnType().getMethod("getRefreshRate");

        Object display = displayClass.getConstructor().newInstance();
        require((Float) refreshRate.invoke(display) == 60.0f,
                "a display reports 60 Hz until Eclipse publishes a monitor");
        require(Arrays.equals((float[]) supportedRefreshRates.invoke(display), new float[] {60.0f}),
                "a display supports only 60 Hz until Eclipse publishes a monitor");

        float[] monitor = {60.0f, 120.0f, 143.996f};
        setRefreshRates.invoke(null, 143.996f, new float[] {60.0f, 120.0f, 143.996f});
        require((Float) refreshRate.invoke(display) == 143.996f,
                "getRefreshRate reports the published monitor");
        float[] supported = (float[]) supportedRefreshRates.invoke(display);
        require(Arrays.equals(supported, monitor),
                "getSupportedRefreshRates reports the published monitor");
        supported[0] = 1.0f;
        require(Arrays.equals((float[]) supportedRefreshRates.invoke(display), monitor),
                "callers receive a copy of the supported rates");
        require((Float) modeRefreshRate.invoke(currentMode.invoke(display)) == 143.996f,
                "the current mode reports the published rate");
        Object[] modes = (Object[]) supportedModes.invoke(display);
        require(modes.length == 1 && (Float) modeRefreshRate.invoke(modes[0]) == 143.996f,
                "the supported modes report the published rate");
        Object otherDisplay = displayClass.getConstructor().newInstance();
        require((Float) refreshRate.invoke(otherDisplay) == 143.996f,
                "every Display reports the same published monitor");

        System.out.println("display-refresh-rates-ok");
        Runtime.getRuntime().halt(0);
    }
}
