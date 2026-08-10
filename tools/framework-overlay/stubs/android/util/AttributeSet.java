
package android.util;
public interface AttributeSet {
  String getAttributeValue(String namespace, String name);
  int getAttributeResourceValue(String namespace, String attribute, int defaultValue);
}
