#ifndef CATALOGMODULE_H
#define CATALOGMODULE_H

#include <QMetaType>

#include <cctype>
#include <string>
#include <vector>

/// One entry of the orchestrator's module catalog — the payload of a `modules` message
/// (orchestrator/src/messages.rs; refactor_design/HANDOVER-client-discovery.md).
///
/// Deliberately lightweight: ribbon metadata (title, icon, menu) is NOT on the wire — the module
/// machinery parses it from the module's Description.qml at `baseUri`. Fields may be added later
/// (e.g. version tie-break info); parsing stays tolerant (see JaspClient::parseCatalog).
struct CatalogModule
{
	std::string name,		///< module name (Package from DESCRIPTION)
				version,	///< module version (Version from DESCRIPTION)
				baseUri;	///< asset directory, trailing slash — file:// on desktop, http:// for the webapp later
};

using ModuleCatalog = std::vector<CatalogModule>;

/// Convert a `file://` base_uri (RFC 8089) to a local directory path (trailing slash
/// preserved), decoding percent-escapes (`%XX`) — the orchestrator's `file_uri` encodes bytes
/// outside RFC 3986's unreserved set (spaces, non-ASCII, ...).
///
/// Authority handling: `file://<host>/<path>` with an EMPTY host — `file:///Users/...`, the
/// normal POSIX/macOS form, and what the orchestrator itself emits (`file://` + absolute path)
/// — and `localhost` are local; any other host is a network share and cannot be mapped to a
/// local directory here. Windows drive letters are restored: `file:///C:/...` arrives as
/// "/C:/..." and the leading slash is dropped. Returns "" for anything unusable as a local
/// path: non-file schemes (`http://` arrives with the webapp phase, where consumers fetch
/// instead of treating it as a local path) or a remote host.
inline std::string fileUriToLocalPath(const std::string & uri)
{
	const std::string scheme = "file://";

	// Scheme match, case-insensitively (RFC 8089: `FILE://` and `file://` are the same).
	if (uri.size() <= scheme.size())
		return "";
	{
		std::string head = uri.substr(0, scheme.size());
		for (char & c : head)
			c = char(std::tolower(static_cast<unsigned char>(c)));
		if (head != scheme)
			return "";
	}

	// Authority: everything up to the next '/' is the host. Empty (the `file:///` form) or
	// "localhost" is local; anything else is a remote share we cannot serve as a local path.
	const size_t pathStart = uri.find('/', scheme.size());
	if (pathStart == std::string::npos)
		return "";
	const std::string host = uri.substr(scheme.size(), pathStart - scheme.size());
	if (!host.empty() && host != "localhost")
		return "";

	// The path proper starts at the '/' after the authority. Windows: "/C:/..." → "C:/...".
	size_t i = pathStart;
	if (i + 2 < uri.size() && std::isalpha(static_cast<unsigned char>(uri[i + 1])) && uri[i + 2] == ':')
		i += 1;

	// Percent-decode (%XX).
	auto hexVal = [](char c) -> int
	{
		if (c >= '0' && c <= '9')	return c - '0';
		if (c >= 'a' && c <= 'f')	return c - 'a' + 10;
		if (c >= 'A' && c <= 'F')	return c - 'A' + 10;
		return -1;
	};

	std::string path;
	path.reserve(uri.size() - i);
	for (; i < uri.size(); ++i)
	{
		if (uri[i] == '%' && i + 2 < uri.size())
		{
			const int hi = hexVal(uri[i + 1]), lo = hexVal(uri[i + 2]);
			if (hi >= 0 && lo >= 0)
			{
				path.push_back(static_cast<char>((hi << 4) | lo));
				i += 2;
				continue;
			}
		}
		path.push_back(uri[i]);
	}

	return path;
}

Q_DECLARE_METATYPE(ModuleCatalog)	///< carried across queued connections / signal-slot

#endif // CATALOGMODULE_H
