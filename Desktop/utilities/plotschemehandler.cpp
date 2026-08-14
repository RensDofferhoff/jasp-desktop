#include "plotschemehandler.h"
#include "tempfiles.h"

PlotSchemeHandler::PlotSchemeHandler(QObject *parent) : QWebEngineUrlSchemeHandler(parent)
{
	QQuickWebEngineProfile::defaultProfile()->installUrlSchemeHandler("plot", this);
}

void PlotSchemeHandler::createUrlScheme()
{
	QWebEngineUrlScheme plotScheme = QWebEngineUrlScheme("plot");
	plotScheme.setFlags(QWebEngineUrlScheme::ContentSecurityPolicyIgnored);
	plotScheme.setSyntax(QWebEngineUrlScheme::Syntax::Path);
	QWebEngineUrlScheme::registerScheme(plotScheme);
}

void PlotSchemeHandler::requestStarted(QWebEngineUrlRequestJob *request)
{
	QUrl	fileUrl		= request->requestUrl();
	QString pathPart	= fileUrl.toString(QUrl::RemoveScheme | QUrl::RemoveQuery);
	//Maybe we could remove the whole ?rev=number thing because we are not caching anything here. But maybe webengine does, Im leaving it for now to avoid too many changes.

	// NEO: the frontend rewrites plot asset paths to absolute ones (the orchestrator's
	// per-revision results dir rides wire-only with each result) — serve those directly.
	// Legacy artifacts stay bare file names resolved against the session temp dir.
	QString stripped = pathPart;
	while (stripped.startsWith('/'))
		stripped = stripped.mid(1);

	QString filePath = stripped.contains('/')
						? "/" + stripped
						: QString::fromStdString(TempFiles::sessionDirName()) + "/" + stripped;

	if(filePath.indexOf(".png") == -1)
	{
		request->fail(QWebEngineUrlRequestJob::Error::UrlInvalid);
		return;
	}

	QFile * png = new QFile(filePath, request);
	if(!png->exists())
	{
		request->fail(QWebEngineUrlRequestJob::Error::UrlNotFound);
		return;
	}
	png->open(QIODevice::ReadOnly);

	request->reply("image/png", png);
}
