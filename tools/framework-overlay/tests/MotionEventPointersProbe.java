import java.lang.reflect.Array;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public final class MotionEventPointersProbe {
    private static final int ACTION_DOWN = 0;
    private static final int ACTION_POINTER_DOWN = 5;
    private static final int ACTION_POINTER_INDEX_SHIFT = 8;
    private static final int SOURCE_TOUCHSCREEN = 0x1002;

    private static Method findPointerIndex;
    private static Method getX;
    private static Method getY;
    private static Method getDownTime;
    private static Method getEventTime;

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static int pointerIndex(Object event, int pointerId) throws ReflectiveOperationException {
        return (Integer) findPointerIndex.invoke(event, pointerId);
    }

    private static long downTime(Object event) throws ReflectiveOperationException {
        return (Long) getDownTime.invoke(event);
    }

    private static boolean rejectsPointerIndex(Method coordinate, Object event, int pointerIndex)
            throws IllegalAccessException {
        try {
            coordinate.invoke(event, pointerIndex);
            return false;
        } catch (InvocationTargetException thrown) {
            return thrown.getCause() instanceof IllegalArgumentException;
        }
    }

    public static void main(String[] arguments) throws ReflectiveOperationException {
        Class<?> motionEvent = Class.forName("android.view.MotionEvent");
        Class<?> propertiesClass = Class.forName("android.view.MotionEvent$PointerProperties");
        Class<?> coordsClass = Class.forName("android.view.MotionEvent$PointerCoords");
        findPointerIndex = motionEvent.getMethod("findPointerIndex", int.class);
        getX = motionEvent.getMethod("getX", int.class);
        getY = motionEvent.getMethod("getY", int.class);
        getDownTime = motionEvent.getMethod("getDownTime");
        getEventTime = motionEvent.getMethod("getEventTime");
        Method getPointerCount = motionEvent.getMethod("getPointerCount");
        Method getActionMasked = motionEvent.getMethod("getActionMasked");
        Method getActionIndex = motionEvent.getMethod("getActionIndex");
        Method getSource = motionEvent.getMethod("getSource");
        Method recycle = motionEvent.getMethod("recycle");

        int[] ids = {0, 2};
        float[][] positions = {{10.0f, 20.0f}, {30.0f, 40.0f}};
        Object properties = Array.newInstance(propertiesClass, ids.length);
        Object coords = Array.newInstance(coordsClass, ids.length);
        for (int index = 0; index < ids.length; index++) {
            Object property = propertiesClass.getConstructor().newInstance();
            propertiesClass.getField("id").setInt(property, ids[index]);
            Array.set(properties, index, property);
            Object coord = coordsClass.getConstructor().newInstance();
            coordsClass.getField("x").setFloat(coord, positions[index][0]);
            coordsClass.getField("y").setFloat(coord, positions[index][1]);
            Array.set(coords, index, coord);
        }
        Method obtainPointers = motionEvent.getMethod("obtain", long.class, long.class, int.class,
                int.class, properties.getClass(), coords.getClass(), int.class, int.class,
                float.class, float.class, int.class, int.class, int.class, int.class);
        Object event = obtainPointers.invoke(null, 1000L, 1016L,
                ACTION_POINTER_DOWN | 1 << ACTION_POINTER_INDEX_SHIFT, ids.length, properties,
                coords, 0, 0, 1.0f, 1.0f, 1, 0, SOURCE_TOUCHSCREEN, 0);

        require((Integer) getPointerCount.invoke(event) == 2, "both pointers are in the event");
        require((Integer) getActionMasked.invoke(event) == ACTION_POINTER_DOWN,
                "the masked action is ACTION_POINTER_DOWN");
        require((Integer) getActionIndex.invoke(event) == 1, "the second pointer went down");
        require((Integer) getSource.invoke(event) == SOURCE_TOUCHSCREEN,
                "the event comes from a touchscreen");
        require(pointerIndex(event, 0) == 0, "pointer id 0 is at index 0");
        require(pointerIndex(event, 2) == 1, "pointer id 2 is at index 1");
        require(pointerIndex(event, 1) == -1, "an absent pointer id has no index");
        require((Float) getX.invoke(event, 1) == 30.0f && (Float) getY.invoke(event, 1) == 40.0f,
                "index 1 reads the second pointer's position");
        require(downTime(event) == 1000L, "the multi-pointer obtain keeps the stream's down time");
        require((Long) getEventTime.invoke(event) == 1016L, "the event keeps its own time");
        require(rejectsPointerIndex(getX, event, -1) && rejectsPointerIndex(getY, event, 2),
                "an out-of-range pointer index throws IllegalArgumentException like Android");

        Object copy = motionEvent.getMethod("obtain", motionEvent).invoke(null, event);
        require(downTime(copy) == 1000L, "a copied event keeps the down time");
        require(pointerIndex(copy, 2) == 1, "a copied event keeps the pointer ids");

        recycle.invoke(event);
        recycle.invoke(copy);
        Method obtainSingle = motionEvent.getMethod("obtain", long.class, long.class, int.class,
                float.class, float.class, int.class);
        Object single = obtainSingle.invoke(null, 2000L, 2032L, ACTION_DOWN, 5.0f, 6.0f, 0);
        require(downTime(single) == 2000L, "the single-pointer obtain keeps the down time");
        require((Long) getEventTime.invoke(single) == 2032L, "the single-pointer event time");

        Object constructed = motionEvent
                .getConstructor(int.class, int.class, long.class, float.class, float.class,
                        float.class, float.class)
                .newInstance(SOURCE_TOUCHSCREEN, ACTION_DOWN, 3000L, 1.0f, 2.0f, 1.0f, 2.0f);
        require(downTime(constructed) == 3000L,
                "a constructed event starts its own stream at its event time");

        System.out.println("motion-event-pointers-ok");
        Runtime.getRuntime().halt(0);
    }
}
