import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

public final class ClientFrameworkApisProbe {
    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static Method publicStatic(String className, String name, Class<?> returnType,
            Class<?>... parameters) throws ReflectiveOperationException {
        Method method = Class.forName(className).getMethod(name, parameters);
        int modifiers = method.getModifiers();
        require(Modifier.isPublic(modifiers) && Modifier.isStatic(modifiers),
                className + "." + name + " is public static");
        require(method.getReturnType() == returnType,
                className + "." + name + " returns " + returnType);
        return method;
    }

    private static Method publicInstance(String className, String name, Class<?> returnType,
            Class<?>... parameters) throws ReflectiveOperationException {
        Method method = Class.forName(className).getMethod(name, parameters);
        int modifiers = method.getModifiers();
        require(Modifier.isPublic(modifiers) && !Modifier.isStatic(modifiers),
                className + "." + name + " is a public instance method");
        require(method.getReturnType() == returnType,
                className + "." + name + " returns " + returnType);
        return method;
    }

    private static Object allocate(String className) throws ReflectiveOperationException {
        Class<?> unsafeClass = Class.forName("sun.misc.Unsafe");
        Field theUnsafe = unsafeClass.getDeclaredField("theUnsafe");
        theUnsafe.setAccessible(true);
        return unsafeClass.getMethod("allocateInstance", Class.class)
                .invoke(theUnsafe.get(null), Class.forName(className));
    }

    private static void requirePublicConstructor(String className)
            throws ReflectiveOperationException {
        Class<?> type = Class.forName(className);
        Object instance = type.getConstructor().newInstance();
        require(instance.getClass() == type, "new " + className + "() constructs a " + className);
    }

    private static void requireConstant(String name, Class<?> returnType, Object expected)
            throws ReflectiveOperationException {
        Object actual = publicStatic("android.view.ViewConfiguration", name, returnType).invoke(null);
        require(expected.equals(actual),
                "ViewConfiguration." + name + " returns " + actual + ", AOSP returns " + expected);
    }

    private static void requireOpacity(Method resolveOpacity, int first, int second, int expected)
            throws ReflectiveOperationException {
        Object actual = resolveOpacity.invoke(null, first, second);
        require(Integer.valueOf(expected).equals(actual),
                "Drawable.resolveOpacity(" + first + ", " + second + ") returns " + actual
                        + ", AOSP returns " + expected);
    }

    private static void requireOffset(Method method, CharSequence text, int offset, int expected)
            throws ReflectiveOperationException {
        Object actual = method.invoke(null, text, offset);
        require(Integer.valueOf(expected).equals(actual),
                "TextUtils." + method.getName() + "(" + text + ", " + offset + ") returns "
                        + actual + ", AOSP returns " + expected);
    }

    private static void requireBits(Method getBitsPerPixel, int format, int expected)
            throws ReflectiveOperationException {
        Object actual = getBitsPerPixel.invoke(null, format);
        require(Integer.valueOf(expected).equals(actual),
                "ImageFormat.getBitsPerPixel(0x" + Integer.toHexString(format) + ") returns "
                        + actual + ", AOSP returns " + expected);
    }

    private static void requireMirror(Method getMirror, char character, char expected)
            throws ReflectiveOperationException {
        Object actual = getMirror.invoke(null, character);
        require(Character.valueOf(expected).equals(actual),
                "AndroidCharacter.getMirror(U+" + Integer.toHexString(character) + ") returns "
                        + actual + ", ICU 60 returns " + expected);
    }

    public static void main(String[] arguments) throws ReflectiveOperationException {
        requireConstant("getEdgeSlop", int.class, 12);
        requireConstant("getFadingEdgeLength", int.class, 12);
        requireConstant("getGlobalActionKeyTimeout", long.class, 500L);
        requireConstant("getJumpTapTimeout", int.class, 500);
        requireConstant("getMaximumDrawingCacheSize", int.class, 480 * 800 * 4);
        requireConstant("getMaximumFlingVelocity", int.class, 8000);
        requireConstant("getPressedStateDuration", int.class, 64);
        requireConstant("getScrollBarSize", int.class, 4);
        requireConstant("getTouchSlop", int.class, 8);
        requireConstant("getWindowTouchSlop", int.class, 16);
        requireConstant("getZoomControlsTimeout", long.class, 3000L);

        Object minVolume = publicStatic("android.media.AudioTrack", "getMinVolume", float.class)
                .invoke(null);
        Object maxVolume = publicStatic("android.media.AudioTrack", "getMaxVolume", float.class)
                .invoke(null);
        require(Float.valueOf(0.0f).equals(minVolume), "AudioTrack.getMinVolume returns 0.0");
        require(Float.valueOf(1.0f).equals(maxVolume), "AudioTrack.getMaxVolume returns 1.0");

        Method resolveOpacity = publicStatic("android.graphics.drawable.Drawable",
                "resolveOpacity", int.class, int.class, int.class);
        int unknown = 0;
        int translucent = -3;
        int transparent = -2;
        int opaque = -1;
        int[] opacities = {unknown, translucent, transparent, opaque};
        for (int opacity : opacities) {
            requireOpacity(resolveOpacity, opacity, opacity, opacity);
        }
        requireOpacity(resolveOpacity, unknown, translucent, unknown);
        requireOpacity(resolveOpacity, opaque, unknown, unknown);
        requireOpacity(resolveOpacity, translucent, transparent, translucent);
        requireOpacity(resolveOpacity, opaque, translucent, translucent);
        requireOpacity(resolveOpacity, transparent, opaque, transparent);
        requireOpacity(resolveOpacity, opaque, transparent, transparent);
        requireOpacity(resolveOpacity, opaque, 1, opaque);

        Method before = publicStatic("android.text.TextUtils", "getOffsetBefore", int.class,
                CharSequence.class, int.class);
        Method after = publicStatic("android.text.TextUtils", "getOffsetAfter", int.class,
                CharSequence.class, int.class);
        String emoji = "a\uD83D\uDE00b";
        requireOffset(before, emoji, 0, 0);
        requireOffset(before, emoji, 1, 0);
        requireOffset(before, emoji, 3, 1);
        requireOffset(before, emoji, 4, 3);
        requireOffset(after, emoji, 0, 1);
        requireOffset(after, emoji, 1, 3);
        requireOffset(after, emoji, 3, 4);
        requireOffset(after, emoji, 4, 4);
        requireOffset(before, "x\uDE00", 2, 1);
        requireOffset(after, "\uD83Dx", 0, 1);

        Class<?> spannableClass = Class.forName("android.text.SpannableString");
        Object replaced = spannableClass.getConstructor(CharSequence.class).newInstance("abcde");
        Object replacement = Class.forName("android.text.style.ReplacementSpan")
                .getConstructor().newInstance();
        spannableClass.getMethod("setSpan", Object.class, int.class, int.class, int.class)
                .invoke(replaced, replacement, 1, 3, 33);
        CharSequence spanned = (CharSequence) replaced;
        requireOffset(after, spanned, 0, 1);
        requireOffset(after, spanned, 1, 3);
        requireOffset(after, spanned, 3, 4);
        requireOffset(before, spanned, 3, 1);
        requireOffset(before, spanned, 1, 0);
        requireOffset(before, spanned, 4, 3);

        Method getMirror = publicStatic("android.text.AndroidCharacter", "getMirror", char.class,
                char.class);
        requireMirror(getMirror, '0', '0');
        requireMirror(getMirror, 'a', 'a');
        requireMirror(getMirror, '(', ')');
        requireMirror(getMirror, ']', '[');
        requireMirror(getMirror, '<', '>');
        requireMirror(getMirror, '\u00ab', '\u00bb');
        requireMirror(getMirror, '\u2264', '\u2265');
        requireMirror(getMirror, '\u2208', '\u220b');
        requireMirror(getMirror, '\u2201', '\u2201');

        Method getBitsPerPixel = publicStatic("android.graphics.ImageFormat", "getBitsPerPixel",
                int.class, int.class);
        requireBits(getBitsPerPixel, 0x4, 16);
        requireBits(getBitsPerPixel, 0x10, 16);
        requireBits(getBitsPerPixel, 0x11, 12);
        requireBits(getBitsPerPixel, 0x14, 16);
        requireBits(getBitsPerPixel, 0x20, 16);
        requireBits(getBitsPerPixel, 0x23, 12);
        requireBits(getBitsPerPixel, 0x25, 10);
        requireBits(getBitsPerPixel, 0x26, 12);
        requireBits(getBitsPerPixel, 0x27, 16);
        requireBits(getBitsPerPixel, 0x28, 24);
        requireBits(getBitsPerPixel, 0x29, 24);
        requireBits(getBitsPerPixel, 0x2a, 32);
        requireBits(getBitsPerPixel, 0x1002, 16);
        requireBits(getBitsPerPixel, 0x20203859, 8);
        requireBits(getBitsPerPixel, 0x20363159, 16);
        requireBits(getBitsPerPixel, 0x32315659, 12);
        requireBits(getBitsPerPixel, 0x44363159, 16);
        requireBits(getBitsPerPixel, 0, -1);
        requireBits(getBitsPerPixel, 0x22, -1);
        requireBits(getBitsPerPixel, 0x24, -1);
        requireBits(getBitsPerPixel, 0x100, -1);
        requireBits(getBitsPerPixel, 0x101, -1);

        Class<?> cdma = Class.forName("android.telephony.cdma.CdmaCellLocation");
        require(cdma.getSuperclass() == Class.forName("android.telephony.CellLocation"),
                "CdmaCellLocation extends CellLocation");
        Method convert = publicStatic(cdma.getName(), "convertQuartSecToDecDegrees", double.class,
                int.class);
        require(Double.valueOf(0.0).equals(convert.invoke(null, 0)),
                "convertQuartSecToDecDegrees(0) returns 0.0");
        require(Double.valueOf(1.0).equals(convert.invoke(null, 14400)),
                "convertQuartSecToDecDegrees(14400) returns 1.0");
        require(Double.valueOf(-180.0).equals(convert.invoke(null, -2592000)),
                "convertQuartSecToDecDegrees(-2592000) returns -180.0");
        require(Double.valueOf(180.0).equals(convert.invoke(null, 2592000)),
                "convertQuartSecToDecDegrees(2592000) returns 180.0");
        for (int invalid : new int[] {2592001, -2592001}) {
            try {
                convert.invoke(null, invalid);
                throw new AssertionError("convertQuartSecToDecDegrees(" + invalid + ") returned");
            } catch (InvocationTargetException thrown) {
                Throwable cause = thrown.getCause();
                require(cause instanceof IllegalArgumentException
                                && ("Invalid coordiante value:" + invalid)
                                        .equals(cause.getMessage()),
                        "convertQuartSecToDecDegrees(" + invalid
                                + ") throws AOSP's IllegalArgumentException, not " + cause);
            }
        }

        for (String className : new String[] {"android.text.AndroidCharacter",
                "android.graphics.ImageFormat", "android.telephony.cdma.CdmaCellLocation"}) {
            requirePublicConstructor(className);
        }

        Object linkProperties = Class.forName("android.net.LinkProperties").getConstructor()
                .newInstance();
        require(publicInstance("android.net.LinkProperties", "getPrivateDnsServerName",
                        String.class).invoke(linkProperties) == null,
                "LinkProperties without private DNS has no private DNS server name");
        require(Boolean.FALSE.equals(publicInstance("android.net.LinkProperties",
                        "isPrivateDnsActive", boolean.class).invoke(linkProperties)),
                "LinkProperties without private DNS reports private DNS inactive");

        Object telephony = Class.forName("android.telephony.TelephonyManager").getConstructor()
                .newInstance();
        require(Integer.valueOf(0).equals(publicInstance("android.telephony.TelephonyManager",
                        "getDataState", int.class).invoke(telephony)),
                "TelephonyManager.getDataState reports DATA_DISCONNECTED without cellular data");

        Class<?> motionEventClass = Class.forName("android.view.MotionEvent");
        Method unbuffered = publicInstance("android.view.View", "requestUnbufferedDispatch",
                void.class, motionEventClass);
        require(Modifier.isFinal(unbuffered.getModifiers()),
                "View.requestUnbufferedDispatch is final");
        Object view = allocate("android.view.View");
        Object down = motionEventClass.getMethod("obtain", long.class, long.class, int.class,
                float.class, float.class, int.class).invoke(null, 0L, 0L, 0, 1.0f, 2.0f, 0);
        unbuffered.invoke(view, down);
        try {
            unbuffered.invoke(view, (Object) null);
            throw new AssertionError("View.requestUnbufferedDispatch(null) returned");
        } catch (InvocationTargetException thrown) {
            require(thrown.getCause() instanceof NullPointerException,
                    "View.requestUnbufferedDispatch(null) throws NullPointerException, not "
                            + thrown.getCause());
        }

        System.out.println("client-framework-apis-ok");
        Runtime.getRuntime().halt(0);
    }
}
