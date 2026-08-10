
package org.xmlpull.v1;
public interface XmlPullParser {
	int START_TAG = 2;
	int END_TAG = 3;
	int END_DOCUMENT = 1;
	int next() throws Exception;
	int getDepth();
	String getName();
	String getPositionDescription();
}
