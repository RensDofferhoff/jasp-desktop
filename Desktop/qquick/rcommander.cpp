#include "rcommander.h"
#include "mainwindow.h"
#include "modules/dynamicmodules.h"

RCommander * RCommander::_lastCommander = nullptr;

RCommander::RCommander()
{
	_lastCommander = this;

	// NEO gut: the R-commander's dedicated engine (createRCmdEngine) is gone. The R prompt will be
	// re-routed over the orchestrator as an `rcode` work-unit (see GUT_TODO.md); inert until then.

	connect(DataSetPackage::pkg(),	&DataSetPackage::currentFileChanged,		this, [&](){ RCommander::_wdWasSet = false;  });

	_scrollTimer = new QTimer(this);
	_scrollTimer->setInterval(50);
	_scrollTimer->setSingleShot(true);

	connect(_scrollTimer,	&QTimer::timeout, this, &RCommander::scrollDown);
	_scrollTimer->start();

	connect(MainWindow::singleton(), &MainWindow::closeWindows, this, &RCommander::closeWindow);
}

RCommander::~RCommander()
{
	if(_lastCommander == this)
		_lastCommander = nullptr;
}

void RCommander::makeActive()
{
	emit _lastCommander->activated();
}

bool RCommander::runCode(const QString & code)
{
	// NEO gut: no engine to run on yet; the R prompt is inert (see GUT_TODO.md).
	Log::log() << "RCommander::runCode ignored (engine removed): " << fq(code) << std::endl;
	return false;
}

bool RCommander::parseAnalysisCode(const QString& code, QString& moduleName, QString& analysisName) const
{
	// Check whether the code starts with '<moduleName>::<analysisName>(...)'
	QString codeTrimmed = code.trimmed();
	int startBracket = codeTrimmed.indexOf('(');

	if (startBracket < 0) return false;
	if (codeTrimmed.indexOf(')', startBracket) < 0 ) return false;

	QStringList analysisParts = codeTrimmed.mid(0, startBracket).split("::");
	if (analysisParts.length() != 2) return false;

	moduleName = analysisParts[0];
	analysisName = analysisParts[1];

	Modules::DynamicModule* module = DynamicModules::dynMods()->dynamicModule(moduleName);
	if (!module) return false;

	for (const Modules::AnalysisEntry* entry : module->menu())
		if (entry->isAnalysis() && entry->isEnabled() && entry->hasWrapper() && entry->function() == fq(analysisName))
			return true;

	return false;
}

bool RCommander::addAnalysis(const QString &code)
{
	// NEO gut: analysis creation via the R prompt depended on the engine path; inert for now.
	Log::log() << "RCommander::addAnalysis ignored (engine removed): " << fq(code) << std::endl;
	return false;
}

void RCommander::setIsAnalysisCode(bool isAnalysisCode)
{
	if (_isAnalysisCode == isAnalysisCode)
		return;

	_isAnalysisCode = isAnalysisCode;

	emit isAnalysisCodeChanged(_isAnalysisCode);
}

void RCommander::checkRCode(const QString &code)
{
	QString moduleName, analysisName;
	setIsAnalysisCode(parseAnalysisCode(code, moduleName, analysisName));
}

void RCommander::rCodeReturned(const QString & result, int, bool)
{
	appendToOutput(result);

	setRunning(false);
}

void RCommander::rCodeReturnedLog(const QString & log, bool)
{
	appendToOutput(log);

	setRunning(false);
}

void RCommander::setRunning(bool running)
{
	if (_running == running)
		return;

	_running = running;
	emit runningChanged(_running);
}

void RCommander::setLastCmd(QString lastCmd)
{
	if (_lastCmd == lastCmd)
		return;

	_lastCmd = lastCmd;
	emit lastCmdChanged(_lastCmd);
}

void RCommander::countDownToScroll()
{
	_scrollTimer->start();
}

void RCommander::setOutput(const QString & output)
{
	if (_output == output)
		return;

	_output = output;
	emit outputChanged(_output);

	_scrollTimer->start(); //I had some trouble getting the scrolldown thing to work easily, this workaround seems good.
}

void RCommander::loadModule(const QString & moduleName)
{
	// NEO gut: module loading for the R prompt went through a dedicated engine; inert for now.
	Log::log() << "RCommander::loadModule ignored (engine removed): " << fq(moduleName) << std::endl;
}

void RCommander::processEngineChanges()
{
	// NEO gut: no engine state to reflect anymore.
}
