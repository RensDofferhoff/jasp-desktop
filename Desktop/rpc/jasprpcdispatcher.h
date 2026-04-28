//
// JaspRpcDispatcher - Central JSON-RPC 2.0 dispatch hub for JASP.
//
// Maps incoming method strings to C++ handlers using a registry pattern.
// Handlers are std::function<Json::Value(const Json::Value& params)>.
// dispatch() is safe to call from the Qt event loop; handlers run
// synchronously so must not block for long periods.
//
// In addition to the low-level registerMethod(string, handler) the
// dispatcher accepts OpenRPC-inspired method specs that describe
// parameter / result schemas.  When a spec is provided the dispatcher
// automatically validates incoming params against the schema, fills in
// any declared default values, and validates the handler's return value
// before sending it to the client.
//

#ifndef JASPRPCDISPATCHER_H
#define JASPRPCDISPATCHER_H

#include <functional>
#include <memory>
#include <string>
#include <unordered_map>
#include <vector>

#include "json/json.h"

// =========================================================================
//  JSON-Schema leaf types (used inside RPC specs)
// =========================================================================

/// A JSON Schema fragment describing a single value (recursive for objects).
struct RpcSchema
{
	std::string   type;          // "string","integer","number","boolean","object","array","null","any"
	std::string   description;   // human-readable
	Json::Value   defaultValue;  // Json::nullValue = no default

	/// Only meaningful when type == "object".
	std::vector<std::string> required;

	/// One named property inside an object schema.
	struct Property
	{
		Property() = default;
		Property(Property&&) = default;
		Property& operator=(Property&&) = default;

		Property(const Property& o)
			: name(o.name), description(o.description)
			, required(o.required), defaultValue(o.defaultValue)
			, schema(o.schema ? std::make_unique<RpcSchema>(*o.schema) : nullptr)
		{}
		Property& operator=(const Property& o)
		{
			if (this != &o)
			{
				name = o.name; description = o.description;
				required = o.required; defaultValue = o.defaultValue;
				schema = o.schema ? std::make_unique<RpcSchema>(*o.schema) : nullptr;
			}
			return *this;
		}

		std::string                 name;
		std::string                 description;
		bool                        required     = false;
		Json::Value                 defaultValue;
		std::unique_ptr<RpcSchema>  schema;  // nullptr = accept any
	};

	std::vector<Property> properties;

	// ---- factories ----------------------------------------------------
	static RpcSchema fromJson(const Json::Value& json);
	static RpcSchema any();           // accepts everything
};

// =========================================================================
//  Parameter / result descriptors
// =========================================================================

/// Describes one named parameter of an RPC method.
struct RpcParamSpec
{
	std::string name;
	std::string description;
	bool        required = true;
	RpcSchema   schema;          // RpcSchema::any() = no type check
};

/// Describes the return value of an RPC method.
struct RpcResultSpec
{
	std::string name;
	std::string description;
	RpcSchema   schema;
};

// =========================================================================
//  Full method specification (OpenRPC-inspired)
// =========================================================================

/// An OpenRPC-ish method descriptor.  Can be built directly in C++ or
/// parsed from a JSON string / Json::Value.
struct RpcMethodSpec
{
	std::string               name;
	std::string               summary;
	std::vector<RpcParamSpec> params;
	RpcResultSpec             result;

	// ---- factories ----------------------------------------------------
	/// Parse a JSON object that follows the OpenRPC method-spec shape.
	/// Throws std::runtime_error on malformed input.
	static RpcMethodSpec fromJson(const Json::Value& json);

	/// Convenience: parse from a raw JSON string.
	static RpcMethodSpec fromJsonString(const std::string& jsonStr);
};

// =========================================================================
//  Handler type
// =========================================================================

using RpcHandler = std::function<Json::Value(const Json::Value& params)>;

// =========================================================================
//  Dispatcher
// =========================================================================

class JaspRpcDispatcher
{
public:
	JaspRpcDispatcher();
	~JaspRpcDispatcher();

	static JaspRpcDispatcher* singleton() { return _singleton; }

	// ------------------------------------------------------------------
	// Registration — low level (no automatic validation)
	// ------------------------------------------------------------------

	/// Register a bare handler.  No param/result validation is performed.
	/// Returns false if the method name is already taken.
	bool registerMethod(const std::string& method, RpcHandler handler);

	// ------------------------------------------------------------------
	// Registration — flat param spec (backward-compatible)
	// ------------------------------------------------------------------

	/// Register with a flat (non-nested) list of parameter descriptors.
	/// Missing required params → error; missing optional → filled with null.
	bool registerMethod(const std::string& method,
						std::vector<RpcParamSpec> paramSpec,
						RpcHandler handler);

	// ------------------------------------------------------------------
	// Registration — full OpenRPC method spec
	// ------------------------------------------------------------------

	/// Register together with an OpenRPC-style spec.  The wrapper:
	///   1. Validates incoming `params` against the declared schemas.
	///   2. Applies declared `default` values for missing optional params.
	///   3. Calls the handler with the validated, defaulted params.
	///   4. Validates the handler's return value against `result.schema`.
	/// Returns false if the method name is already registered.
	bool registerMethod(const RpcMethodSpec& spec, RpcHandler handler);

	/// Convenience: parse `specJson` then call registerMethod(RpcMethodSpec, RpcHandler).
	/// Example:
	///   disp->registerMethodFromSpec(R"({"name":"foo","params":[...],"result":{...}})", handler);
	bool registerMethodFromSpec(const std::string& specJson, RpcHandler handler);

	// ------------------------------------------------------------------
	// Registration — by name (from the pre-loaded spec registry)
	// ------------------------------------------------------------------

	/// Look up `methodName` in the internal spec registry (populated by
	/// loadSpecFile / loadSpecFromString), wrap the handler with the same
	/// 4-step pipeline as registerMethod(RpcMethodSpec, handler).
	///
	/// Returns false if the name is not found in the spec registry or the
	/// handler slot is already taken.
	bool registerMethodByName(const std::string& methodName, RpcHandler handler);

	// ------------------------------------------------------------------
	// Spec-file loading
	// ------------------------------------------------------------------

	/// Load an OpenRPC 1.x document from a JSON string.
	/// Every entry in the "methods" array is parsed into the internal spec
	/// registry so that registerMethodByName() can find it later.
	/// Returns the number of methods loaded, or -1 on parse error.
	int loadSpecFromString(const std::string& openRpcJson);

	/// Load an OpenRPC 1.x document from a file on disk.
	/// Reads the file, then delegates to loadSpecFromString.
	int loadSpecFile(const std::string& path);

	/// Return the method names currently in the spec registry.
	std::vector<std::string> knownSpecNames() const;

	// ------------------------------------------------------------------
	// Unregistration / introspection
	// ------------------------------------------------------------------

	void unregisterMethod(const std::string& method);
	std::vector<std::string> registeredMethods() const;

	// ------------------------------------------------------------------
	// Dispatch
	// ------------------------------------------------------------------

	std::string dispatch(const std::string& requestJson);
	Json::Value dispatch(const Json::Value& request);

	// ------------------------------------------------------------------
	// Static helpers — application-level payloads
	// ------------------------------------------------------------------

	static Json::Value successResult();                        // {"status":"success"}
	static Json::Value errorResult(const std::string& message); // {"status":"error","message":"..."}

	// ------------------------------------------------------------------
	// Static helpers — validation (callable from any handler)
	// ------------------------------------------------------------------

	/// Validate `params` against an array of parameter specs.
	/// Returns Json::nullValue on success, or an error object on failure.
	static Json::Value validateParams(const Json::Value& params,
									  const std::vector<RpcParamSpec>& spec);

	/// Validate a single value against a schema.
	static Json::Value validateSchema(const Json::Value& value,
									   const RpcSchema& schema);

	/// Validate a handler's return value against a result spec.
	static Json::Value validateResult(const Json::Value& result,
									  const RpcResultSpec& spec);

	/// Apply declared defaults from a param spec.  Does NOT check required
	/// fields — call validateParams() first.
	static Json::Value applyDefaults(const Json::Value& params,
									 const std::vector<RpcParamSpec>& spec);

private:
	static Json::Value makeError(int code, const std::string& message,
								 const Json::Value& id);
	static Json::Value makeResponse(const Json::Value& result,
									const Json::Value& id);

	static JaspRpcDispatcher* _singleton;
	std::unordered_map<std::string, RpcHandler> _handlers;

	/// Spec registry: method name -> parsed RpcMethodSpec.
	std::unordered_map<std::string, RpcMethodSpec> _specs;
};

#endif // JASPRPCDISPATCHER_H
