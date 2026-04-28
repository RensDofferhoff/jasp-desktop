//
// JaspRpcDispatcher implementation — see jasprpcdispatcher.h for API docs.
//

#include "jasprpcdispatcher.h"
#include <cassert>
#include <fstream>
#include "log.h"
#include "dirs.h"
#include <sstream>

// =========================================================================
//  Internal helpers
// =========================================================================

namespace
{

const char* jsonTypeName(Json::ValueType t)
{
	switch (t)
	{
	case Json::nullValue:    return "null";
	case Json::intValue:     return "integer";
	case Json::uintValue:    return "integer";
	case Json::realValue:    return "number";
	case Json::stringValue:  return "string";
	case Json::booleanValue: return "boolean";
	case Json::arrayValue:   return "array";
	case Json::objectValue:  return "object";
	}
	return "unknown";
}

bool typeMatches(Json::ValueType actual, const std::string& schemaType)
{
	if (schemaType.empty() || schemaType == "any") return true;

	if (schemaType == "string")  return actual == Json::stringValue;
	if (schemaType == "integer") return actual == Json::intValue  ||
	                                     actual == Json::uintValue;
	if (schemaType == "number")  return actual == Json::intValue  ||
	                                     actual == Json::uintValue ||
	                                     actual == Json::realValue;
	if (schemaType == "boolean") return actual == Json::booleanValue;
	if (schemaType == "object")  return actual == Json::objectValue;
	if (schemaType == "array")   return actual == Json::arrayValue;
	if (schemaType == "null")    return actual == Json::nullValue;

	return true; // unknown type name → permissive
}

/// Recursively apply property defaults inside an object value.
Json::Value applySchemaDefaults(const Json::Value& value,
								const RpcSchema& schema)
{
	if (!value.isObject() || schema.properties.empty())
		return value;

	Json::Value out = value;
	for (const auto& prop : schema.properties)
	{
		if (!out.isMember(prop.name) && !prop.defaultValue.isNull())
			out[prop.name] = prop.defaultValue;

		if (out.isMember(prop.name) && prop.schema)
			out[prop.name] = applySchemaDefaults(out[prop.name], *prop.schema);
	}
	return out;
}

/// Returns a JSON error object with code -32602 and the given message.
Json::Value invalidParamsError(const std::string& msg)
{
	Json::Value err;
	err["code"]    = -32602;
	err["message"] = "Invalid params: " + msg;
	return err;
}

} // anonymous namespace

// =========================================================================
//  RpcSchema
// =========================================================================

RpcSchema RpcSchema::fromJson(const Json::Value& json)
{
	RpcSchema s;
	s.type         = json.get("type", "").asString();
	s.description  = json.get("description", "").asString();
	s.defaultValue = json.get("default", Json::nullValue);

	// ---- required list (only meaningful for objects) ------------------
	if (json.isMember("required") && json["required"].isArray())
		for (const auto& r : json["required"])
			s.required.push_back(r.asString());

	// ---- properties ---------------------------------------------------
	if (json.isMember("properties") && json["properties"].isObject())
	{
		const Json::Value& props = json["properties"];
		for (const auto& name : props.getMemberNames())
		{
			Property prop;
			prop.name         = name;
			prop.description  = props[name].get("description", "").asString();
			prop.defaultValue = props[name].get("default",     Json::nullValue);

			// Per-property required flag: true if name appears in
			// the parent's "required" array.
			for (const auto& req : s.required)
				if (req == name) { prop.required = true; break; }

			// Recurse: the property value may itself be a schema object.
			const Json::Value& propJson = props[name];
			if (propJson.isMember("type") || propJson.isMember("properties") ||
				propJson.isMember("required"))
			{
				prop.schema = std::make_unique<RpcSchema>(RpcSchema::fromJson(propJson));
			}

			s.properties.push_back(std::move(prop));
		}
	}

	return s;
}

RpcSchema RpcSchema::any()
{
	RpcSchema s;
	s.type = "any";
	return s;
}

// =========================================================================
//  RpcMethodSpec
// =========================================================================

RpcMethodSpec RpcMethodSpec::fromJson(const Json::Value& json)
{
	if (!json.isObject())
		throw std::runtime_error("RpcMethodSpec must be a JSON object");

	RpcMethodSpec spec;

	// ---- name ---------------------------------------------------------
	if (!json.isMember("name") || !json["name"].isString())
		throw std::runtime_error("RpcMethodSpec: missing 'name' (string)");
	spec.name = json["name"].asString();

	// ---- summary ------------------------------------------------------
	spec.summary = json.get("summary", "").asString();

	// ---- params -------------------------------------------------------
	if (!json.isMember("params") || !json["params"].isArray())
		throw std::runtime_error("RpcMethodSpec '" + spec.name +
								 "': missing 'params' (array)");

	for (const auto& pJson : json["params"])
	{
		if (!pJson.isObject())
			throw std::runtime_error("RpcMethodSpec '" + spec.name +
									 "': each param must be an object");

		RpcParamSpec p;
		if (!pJson.isMember("name") || !pJson["name"].isString())
			throw std::runtime_error("RpcMethodSpec '" + spec.name +
									 "': param missing 'name'");
		p.name        = pJson["name"].asString();
		p.description = pJson.get("description", "").asString();
		p.required    = pJson.get("required", true).asBool();

		if (pJson.isMember("schema"))
			p.schema = RpcSchema::fromJson(pJson["schema"]);
		// else: RpcSchema default = any type accepted

		spec.params.push_back(std::move(p));
	}

	// ---- result -------------------------------------------------------
	if (json.isMember("result") && json["result"].isObject())
	{
		const Json::Value& rJson = json["result"];
		spec.result.name        = rJson.get("name", "").asString();
		spec.result.description = rJson.get("description", "").asString();
		if (rJson.isMember("schema"))
			spec.result.schema = RpcSchema::fromJson(rJson["schema"]);
	}

	return spec;
}

RpcMethodSpec RpcMethodSpec::fromJsonString(const std::string& jsonStr)
{
	Json::Value  root;
	Json::Reader reader;
	if (!reader.parse(jsonStr, root))
		throw std::runtime_error(
			"RpcMethodSpec parse error: " + reader.getFormattedErrorMessages());
	return fromJson(root);
}

// =========================================================================
//  JaspRpcDispatcher — singleton
// =========================================================================

JaspRpcDispatcher* JaspRpcDispatcher::_singleton = nullptr;

JaspRpcDispatcher::JaspRpcDispatcher()
{
	assert(!_singleton);
	_singleton = this;

	// Auto-load the OpenRPC spec file from the Resources directory.
	std::string specPath = Dirs::resourcesDir() + "JASP_RPC.json";
	int n = loadSpecFile(specPath);
	if (n > 0)
		Log::log() << "[JaspRpcDispatcher] Loaded " << n
				  << " method specs from " << specPath << std::endl;
	else if (n == 0)
		Log::log() << "[" << specPath << "] found but no methods loaded"
				  << std::endl;
	// n < 0 → file not found / parse error; silently OK.
}

JaspRpcDispatcher::~JaspRpcDispatcher()
{
	_singleton = nullptr;
}

// =========================================================================
//  Static validation helpers
// =========================================================================

Json::Value JaspRpcDispatcher::validateSchema(const Json::Value& value,
											  const RpcSchema& schema)
{
	// ---- type check ---------------------------------------------------
	if (!schema.type.empty() && schema.type != "any")
	{
		if (!typeMatches(value.type(), schema.type))
		{
			std::ostringstream msg;
			msg << "type mismatch: expected " << schema.type
				<< ", got " << jsonTypeName(value.type());
			return invalidParamsError(msg.str());
		}
	}

	// ---- object: check required properties & recurse ------------------
	if (value.isObject() && !schema.properties.empty())
	{
		for (const auto& prop : schema.properties)
		{
			if (prop.required && !value.isMember(prop.name))
				return invalidParamsError("missing required property: '" +
										  prop.name + "'");

			if (value.isMember(prop.name) && prop.schema)
			{
				Json::Value nested = validateSchema(value[prop.name], *prop.schema);
				if (!nested.isNull())
					return nested; // propagate inner error
			}
		}
	}

	return Json::nullValue;
}

Json::Value JaspRpcDispatcher::validateParams(
	const Json::Value& params,
	const std::vector<RpcParamSpec>& spec)
{
	if (spec.empty())
		return Json::nullValue;

	// Top-level params should be a JSON object (JSON-RPC by-name style).
	if (!params.isObject())
		return invalidParamsError("params must be a JSON object");

	for (const auto& p : spec)
	{
		if (p.required && !params.isMember(p.name))
			return invalidParamsError("missing required param: '" + p.name + "'");

		if (params.isMember(p.name))
		{
			// Only validate against the schema if one is declared (type != "" or properties exist)
			const RpcSchema& sch = p.schema;
			if (!sch.type.empty() || !sch.properties.empty())
			{
				Json::Value err = validateSchema(params[p.name], sch);
				if (!err.isNull())
					return err;
			}
		}
	}

	return Json::nullValue;
}

Json::Value JaspRpcDispatcher::validateResult(const Json::Value& result,
											  const RpcResultSpec& spec)
{
	// Empty result spec → skip validation.
	if (spec.schema.type.empty() && spec.schema.properties.empty())
		return Json::nullValue;

	return validateSchema(result, spec.schema);
}

// =========================================================================
//  Static default-application helpers
// =========================================================================

Json::Value JaspRpcDispatcher::applyDefaults(
	const Json::Value& params,
	const std::vector<RpcParamSpec>& spec)
{
	Json::Value out = params.isObject() ? params : Json::Value(Json::objectValue);

	for (const auto& p : spec)
	{
		// Top-level param default
		if (!out.isMember(p.name) && !p.schema.defaultValue.isNull())
			out[p.name] = p.schema.defaultValue;

		// Recurse into the value if it's an object with property defaults
		if (out.isMember(p.name))
			out[p.name] = applySchemaDefaults(out[p.name], p.schema);
	}

	return out;
}

// =========================================================================
//  Convenience builders
// =========================================================================

Json::Value JaspRpcDispatcher::successResult()
{
	Json::Value r;
	r["status"] = "success";
	return r;
}

Json::Value JaspRpcDispatcher::errorResult(const std::string& message)
{
	Json::Value r;
	r["status"]  = "error";
	r["message"] = message;
	return r;
}

// =========================================================================
//  Registration
// =========================================================================

bool JaspRpcDispatcher::registerMethod(const std::string& method,
									   RpcHandler handler)
{
	if (_handlers.find(method) != _handlers.end())
		return false;

	_handlers[method] = std::move(handler);
	Log::log() << "[JaspRpcDispatcher] Registered: " << method << std::endl;
	return true;
}

bool JaspRpcDispatcher::registerMethod(const RpcMethodSpec& spec,
									   RpcHandler handler)
{
	if (_handlers.find(spec.name) != _handlers.end())
		return false;

	// Wrap the handler: validate params → apply defaults → call → validate result.
	auto wrapped = [spec, fn = std::move(handler)](const Json::Value& params) -> Json::Value
	{
		// 1. Validate params
		Json::Value err = validateParams(params, spec.params);
		if (!err.isNull())
			return err;

		// 2. Apply defaults
		Json::Value safeParams = applyDefaults(params, spec.params);

		// 3. Call the handler
		Json::Value result = fn(safeParams);

		// 4. Validate result (skip if result spec has no schema)
		err = validateResult(result, spec.result);
		if (!err.isNull())
			return err;

		return result;
	};

	_handlers[spec.name] = std::move(wrapped);
	Log::log() << "[JaspRpcDispatcher] Registered (spec): " << spec.name << std::endl;
	return true;
}

bool JaspRpcDispatcher::registerMethodFromSpec(const std::string& specJson,
											   RpcHandler handler)
{
	return registerMethod(RpcMethodSpec::fromJsonString(specJson),
						  std::move(handler));
}

bool JaspRpcDispatcher::registerMethodByName(const std::string& methodName,
											 RpcHandler handler)
{
	auto it = _specs.find(methodName);
	if (it == _specs.end())
	{
		Log::log() << "[JaspRpcDispatcher] registerMethodByName: unknown method '"
				  << methodName << "'" << std::endl;
		return false;
	}

	return registerMethod(it->second, std::move(handler));
}

// =========================================================================
//  Spec-file loading
// =========================================================================

int JaspRpcDispatcher::loadSpecFromString(const std::string& openRpcJson)
{
	Json::Value  root;
	Json::Reader reader;
	if (!reader.parse(openRpcJson, root))
	{
		Log::log() << "[JaspRpcDispatcher] loadSpecFromString: parse error: "
				  << reader.getFormattedErrorMessages() << std::endl;
		return -1;
	}

	if (!root.isObject() || !root.isMember("methods") || !root["methods"].isArray())
	{
		Log::log() << "[JaspRpcDispatcher] loadSpecFromString: missing 'methods' array"
				  << std::endl;
		return -1;
	}

	int loaded = 0;
	for (const auto& methodJson : root["methods"])
	{
		try
		{
			RpcMethodSpec spec = RpcMethodSpec::fromJson(methodJson);
			if (spec.name.empty())
			{
				Log::log() << "[JaspRpcDispatcher] loadSpecFromString: skipping method "
							 "with empty name" << std::endl;
				continue;
			}
			Log::log() << "[JaspRpcDispatcher] Spec loaded: " << spec.name << std::endl;
			_specs[spec.name] = std::move(spec);
			++loaded;
		}
		catch (const std::exception& e)
		{
			Log::log() << "[JaspRpcDispatcher] loadSpecFromString: skipping malformed "
						 "method: " << e.what() << std::endl;
		}
	}

	return loaded;
}

int JaspRpcDispatcher::loadSpecFile(const std::string& path)
{
	std::ifstream file(path);
	if (!file.is_open())
	{
		// Not an error — the file may simply not exist yet.
		return -1;
	}

	std::string content((std::istreambuf_iterator<char>(file)),
						 std::istreambuf_iterator<char>());
	return loadSpecFromString(content);
}

std::vector<std::string> JaspRpcDispatcher::knownSpecNames() const
{
	std::vector<std::string> names;
	names.reserve(_specs.size());
	for (const auto& p : _specs)
		names.push_back(p.first);
	return names;
}

bool JaspRpcDispatcher::registerMethod(const std::string& method,
									   std::vector<RpcParamSpec> paramSpec,
									   RpcHandler handler)
{
	// Backward-compatible overload: wrap with flat required-param check
	// and fill null defaults for missing optional params.
	return registerMethod(method, [spec = std::move(paramSpec),
								   fn = std::move(handler)](const Json::Value& params) -> Json::Value
	{
		Json::Value err = validateParams(params, spec);
		if (!err.isNull())
			return err;

		Json::Value safeParams(Json::objectValue);
		for (const auto& p : spec)
		{
			if (params.isMember(p.name))
				safeParams[p.name] = params[p.name];
			else
				safeParams[p.name] = Json::Value(Json::nullValue);
		}
		return fn(safeParams);
	});
}

void JaspRpcDispatcher::unregisterMethod(const std::string& method)
{
	_handlers.erase(method);
	Log::log() << "[JaspRpcDispatcher] Unregistered: " << method << std::endl;
}

std::vector<std::string> JaspRpcDispatcher::registeredMethods() const
{
	std::vector<std::string> names;
	names.reserve(_handlers.size());
	for (const auto& p : _handlers)
		names.push_back(p.first);
	return names;
}

// =========================================================================
//  JSON-RPC 2.0 protocol helpers (private)
// =========================================================================

Json::Value JaspRpcDispatcher::makeError(int code, const std::string& message,
										 const Json::Value& id)
{
	Json::Value err;
	err["jsonrpc"] = "2.0";
	err["error"]["code"]    = code;
	err["error"]["message"] = message;
	err["id"] = id;
	return err;
}

Json::Value JaspRpcDispatcher::makeResponse(const Json::Value& result,
											 const Json::Value& id)
{
	Json::Value resp;
	resp["jsonrpc"] = "2.0";
	resp["result"]  = result;
	resp["id"]      = id;
	return resp;
}

// =========================================================================
//  Dispatch
// =========================================================================

Json::Value JaspRpcDispatcher::dispatch(const Json::Value& request)
{
	if (!request.isObject())
		return makeError(-32600, "Invalid Request: not an object", Json::nullValue);

	if (request.get("jsonrpc", "") != "2.0")
		return makeError(-32600, "Invalid Request: jsonrpc != '2.0'",
						 request.get("id", Json::nullValue));

	Json::Value id = request.get("id", Json::nullValue);

	if (!request.isMember("method") || !request["method"].isString())
		return makeError(-32600, "Invalid Request: missing method", id);

	std::string method = request["method"].asString();
	auto it = _handlers.find(method);

	if (it == _handlers.end())
		return makeError(-32601, "Method not found: " + method, id);

	Json::Value params = request.get("params", Json::Value(Json::objectValue));

	try
	{
		return makeResponse(it->second(params), id);
	}
	catch (const std::exception& e)
	{
		return makeError(-32000, std::string("Handler error: ") + e.what(), id);
	}
	catch (...)
	{
		return makeError(-32000, "Unknown handler error", id);
	}
}

std::string JaspRpcDispatcher::dispatch(const std::string& requestJson)
{
	Json::Value  req;
	Json::Reader reader;

	if (!reader.parse(requestJson, req))
		return Json::writeString(
			Json::StreamWriterBuilder(),
			makeError(-32700,
					  "Parse error: " + reader.getFormattedErrorMessages(),
					  Json::nullValue));

	Json::Value resp = dispatch(req);

	Json::StreamWriterBuilder builder;
	builder["indentation"] = "";
	return Json::writeString(builder, resp);
}
