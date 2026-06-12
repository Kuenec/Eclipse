/*
 * 2026-06-11: ECLIPSE PATCH — derived from ATL's api-impl `android.net.NetworkRequest`
 * (Apache-2.0). ATL's `Builder` is an inner (non-static) class with only `build()`;
 * AOSP's is a STATIC nested class with a no-arg constructor and fluent
 * `addCapability(int)`/`addTransportType(int)` — Roblox's jobqueue library calls all
 * three from `ActivitySplash.onCreate` (NoSuchMethodError without this patch).
 * Capability/transport values are accepted and ignored: Eclipse's ConnectivityManager
 * backing reports the network available/unmetered, so the request filter has no consumer.
 */
package android.net;

public class NetworkRequest {

	public static class Builder {

		public Builder() {}

		public Builder addCapability(int capability) {
			return this;
		}

		public Builder addTransportType(int transportType) {
			return this;
		}

		public NetworkRequest build() {
			return new NetworkRequest();
		}
	}
}
