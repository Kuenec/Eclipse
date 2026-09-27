import java.lang.reflect.Method;
import java.util.Arrays;

public final class DisplayRefreshRateProbe {
    private static Method refreshRate;
    private static Method supportedRefreshRates;
    private static Method currentMode;
    private static Method supportedModes;
    private static Method modeId;
    private static Method modeRefreshRate;
    private static Method modeWidth;
    private static Method modeHeight;

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static void requireMonitor(Object display, float current, float[] rates, String monitor)
            throws ReflectiveOperationException {
        require((Float) refreshRate.invoke(display) == current,
                "getRefreshRate reports " + monitor);
        require(Arrays.equals((float[]) supportedRefreshRates.invoke(display), rates),
                "getSupportedRefreshRates reports " + monitor);

        Object[] modes = (Object[]) supportedModes.invoke(display);
        require(modes.length == rates.length, "one supported mode per rate of " + monitor);
        Object mode = currentMode.invoke(display);
        require((Float) modeRefreshRate.invoke(mode) == current,
                "the current mode runs at the rate of " + monitor);
        int matches = 0;
        for (int index = 0; index < modes.length; index++) {
            require((Float) modeRefreshRate.invoke(modes[index]) == rates[index],
                    "supported mode " + index + " runs at a rate of " + monitor);
            require(modeWidth.invoke(modes[index]).equals(modeWidth.invoke(mode))
                            && modeHeight.invoke(modes[index]).equals(modeHeight.invoke(mode)),
                    "supported mode " + index + " has the size of the current mode");
            for (int other = 0; other < index; other++) {
                require(!modeId.invoke(modes[index]).equals(modeId.invoke(modes[other])),
                        "supported modes have distinct ids");
            }
            if (modeId.invoke(modes[index]).equals(modeId.invoke(mode))) {
                require((Float) modeRefreshRate.invoke(modes[index]) == current,
                        "the supported mode with the current id runs at the current rate");
                matches++;
            }
        }
        require(matches == 1, "the current mode is one of the supported modes of " + monitor);
    }

    public static void main(String[] arguments) throws ReflectiveOperationException {
        Class<?> displayClass = Class.forName("android.view.Display");
        refreshRate = displayClass.getMethod("getRefreshRate");
        supportedRefreshRates = displayClass.getMethod("getSupportedRefreshRates");
        currentMode = displayClass.getMethod("getMode");
        supportedModes = displayClass.getMethod("getSupportedModes");
        Class<?> modeClass = currentMode.getReturnType();
        modeId = modeClass.getMethod("getModeId");
        modeRefreshRate = modeClass.getMethod("getRefreshRate");
        modeWidth = modeClass.getMethod("getPhysicalWidth");
        modeHeight = modeClass.getMethod("getPhysicalHeight");
        Method setRefreshRates =
                displayClass.getMethod("setRefreshRates", float.class, float[].class);

        Object display = displayClass.getConstructor().newInstance();
        requireMonitor(display, 60.0f, new float[] {60.0f},
                "Android's 60 Hz default until Eclipse publishes a monitor");

        float[] monitor = {60.0f, 120.0f, 143.996f};
        setRefreshRates.invoke(null, 143.996f, monitor.clone());
        requireMonitor(display, 143.996f, monitor, "the published monitor");
        float[] supported = (float[]) supportedRefreshRates.invoke(display);
        supported[0] = 1.0f;
        require(Arrays.equals((float[]) supportedRefreshRates.invoke(display), monitor),
                "callers receive a copy of the supported rates");
        Object otherDisplay = displayClass.getConstructor().newInstance();
        requireMonitor(otherDisplay, 143.996f, monitor, "the monitor published to every Display");

        float[] published = {60.0f};
        setRefreshRates.invoke(null, 60.0f, published);
        published[0] = 1.0f;
        requireMonitor(display, 60.0f, new float[] {60.0f},
                "the monitor published after moving the window");

        System.out.println("display-refresh-rates-ok");
        Runtime.getRuntime().halt(0);
    }
}
