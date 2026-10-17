import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public final class SurfaceViewCallbacksProbe {
    private static final int WINDOW_FORMAT_RGBA_8888 = 1;

    private static Class<?> viewClass;
    private static Class<?> callbackClass;
    private static Method getHolder;
    private static Method addCallback;
    private static Method removeCallback;
    private static Method surfaceCreated;
    private static Method surfaceChanged;

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static Object newSurfaceView() throws ReflectiveOperationException {
        Class<?> unsafeClass = Class.forName("sun.misc.Unsafe");
        Field theUnsafe = unsafeClass.getDeclaredField("theUnsafe");
        theUnsafe.setAccessible(true);
        Object view = unsafeClass.getMethod("allocateInstance", Class.class)
                .invoke(theUnsafe.get(null), viewClass);
        Field callbacks = viewClass.getDeclaredField("mCallbacks");
        callbacks.setAccessible(true);
        callbacks.set(view, new ArrayList<Object>());
        Constructor<?> holderConstructor =
                Class.forName("android.view.SurfaceView$1").getDeclaredConstructor(viewClass);
        holderConstructor.setAccessible(true);
        Field holder = viewClass.getDeclaredField("mSurfaceHolder");
        holder.setAccessible(true);
        holder.set(view, holderConstructor.newInstance(view));
        return view;
    }

    private static Object callback(final String name, final List<String> heard,
            final Object leaving) {
        InvocationHandler handler = new InvocationHandler() {
            @Override
            public Object invoke(Object proxy, Method method, Object[] arguments)
                    throws ReflectiveOperationException {
                switch (method.getName()) {
                    case "equals":
                        return proxy == arguments[0];
                    case "hashCode":
                        return System.identityHashCode(proxy);
                    case "toString":
                        return name;
                    default:
                        heard.add(name + "." + method.getName());
                        if (leaving != null) {
                            removeCallback.invoke(leaving, proxy);
                        }
                        return null;
                }
            }
        };
        return Proxy.newProxyInstance(callbackClass.getClassLoader(),
                new Class<?>[] {callbackClass}, handler);
    }

    private static void dispatch(Object view, Method event) throws ReflectiveOperationException {
        Object[] arguments = event == surfaceChanged
                ? new Object[] {WINDOW_FORMAT_RGBA_8888, 2560, 1440}
                : new Object[0];
        try {
            event.invoke(view, arguments);
        } catch (InvocationTargetException failure) {
            throw new AssertionError("SurfaceView." + event.getName() + " threw "
                    + failure.getCause(), failure.getCause());
        }
    }

    private static void requireRemovedCallbacksStaySilent() throws ReflectiveOperationException {
        Object view = newSurfaceView();
        Object holder = getHolder.invoke(view);
        List<String> heard = new ArrayList<String>();
        Object destroyedFragment = callback("fragment", heard, null);
        Object game = callback("game", heard, null);
        addCallback.invoke(holder, destroyedFragment);
        addCallback.invoke(holder, game);
        removeCallback.invoke(holder, destroyedFragment);
        dispatch(view, surfaceChanged);
        dispatch(view, surfaceCreated);
        require(heard.equals(Arrays.asList("game.surfaceChanged", "game.surfaceCreated")),
                "a callback removed with removeCallback must hear nothing, as on Android; heard "
                        + heard);
    }

    private static void requireCallbacksMayLeaveDuringDispatch(Method event)
            throws ReflectiveOperationException {
        Object view = newSurfaceView();
        Object holder = getHolder.invoke(view);
        List<String> heard = new ArrayList<String>();
        addCallback.invoke(holder, callback("leaving", heard, holder));
        addCallback.invoke(holder, callback("staying", heard, null));
        dispatch(view, event);
        dispatch(view, event);
        String name = event.getName();
        require(heard.equals(Arrays.asList("leaving." + name, "staying." + name,
                        "staying." + name)),
                "a callback that removes itself in " + name
                        + " must not stop the others, as on Android; heard " + heard);
    }

    public static void main(String[] arguments) throws ReflectiveOperationException {
        viewClass = Class.forName("android.view.SurfaceView");
        callbackClass = Class.forName("android.view.SurfaceHolder$Callback");
        Class<?> holderClass = Class.forName("android.view.SurfaceHolder");
        getHolder = viewClass.getMethod("getHolder");
        addCallback = holderClass.getMethod("addCallback", callbackClass);
        removeCallback = holderClass.getMethod("removeCallback", callbackClass);
        surfaceCreated = viewClass.getDeclaredMethod("surfaceCreated");
        surfaceCreated.setAccessible(true);
        surfaceChanged =
                viewClass.getDeclaredMethod("surfaceChanged", int.class, int.class, int.class);
        surfaceChanged.setAccessible(true);

        requireRemovedCallbacksStaySilent();
        requireCallbacksMayLeaveDuringDispatch(surfaceChanged);
        requireCallbacksMayLeaveDuringDispatch(surfaceCreated);
        System.out.println("surface-view-callbacks-ok");
        Runtime.getRuntime().halt(0);
    }
}
