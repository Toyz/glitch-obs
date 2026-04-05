/*
 * Thin wrapper around the obs-websocket inline API so Rust can call it via FFI.
 * obs-websocket-api.h is header-only (all static inline), so bindgen can't
 * see these symbols — we compile them here and link the resulting .a.
 */

#include <obs.h>
#include <obs-websocket-api.h>

void *ws_shim_register_vendor(const char *name)
{
	return obs_websocket_register_vendor(name);
}

bool ws_shim_register_request(void *vendor, const char *type,
			      obs_websocket_request_callback_function cb,
			      void *priv_data)
{
	return obs_websocket_vendor_register_request(vendor, type, cb,
						     priv_data);
}

bool ws_shim_emit_event(void *vendor, const char *event_name,
			obs_data_t *event_data)
{
	return obs_websocket_vendor_emit_event(vendor, event_name, event_data);
}
