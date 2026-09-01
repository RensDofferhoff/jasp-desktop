//
// Copyright (C) 2013-2018 University of Amsterdam
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 2 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//

#ifndef ANALYSIS_H
#define ANALYSIS_H

// The excision aftermath (2026-09-02): the boost/uuid include died — nothing ever used it.

#include "enginedefinitions.h"

#include <set>
#include "analysisbase.h"
#include "qutils.h"
#include "modules/dynamicmodules.h"
#include <QFileSystemWatcher>
#include <QQuickItem>

class Filter;
class DataSet;
class AnalysisForm;
// The excision, Cut 7: the stale `class Column;` fwd-decl died with Column's last phantom use.


///
/// A single instantiated analysis, aka it was clicked by the user and now has a qml-form loaded and some (rudimentary) output in the results or is on its way there.
/// This has its counterpart in AnysisForm which is the backend of the qml `Form {}` element.
/// Analysis and AnalysisForm together handle most of the interaction between the user and (eventually) R.
/// Status is display-only now (Empty/Running/Complete/error/aborted); work is triggered by an explicit run()/submit, never by setting a status.
/// NEO: commands for the runner are issued through the orchestrator client (see refactor_design/poc-handoff.md),
/// replacing the old in-process engine scheduler.
class Analysis : public AnalysisBase
{
	Q_OBJECT

	friend class Analyses;

	typedef std::map<std::string, std::set<std::string>> optionColumns;

public:

	// NEO: display-only status. The old command-states (SaveImg/EditImg/RewriteImgs/RunningImg/
	// Aborting) and the KeepStatus sentinel died with the engine poll — work is triggered by an
	// explicit run()/submit now, and image ops are deferred.
	enum Status { Empty, Running, Complete, Aborted, ValidationError, FatalError };

	void				setStatus(Status status);
	static std::string	statusToString(Status status);

	///This function transforms an analysisResultStatus to Analysis::Status so that the Analysis gets the correct status after returning from Engine
	static Analysis::Status analysisResultsStatusToAnalysisStatus(analysisResultStatus result);

						Analysis(size_t id, Analysis * duplicateMe);
						Analysis(size_t id, Modules::AnalysisEntry * analysisEntry, const std::string & title, const Version & optionsVersion, const Json::Value & options);
						Analysis(size_t id, const std::string & title); // report constructor — no module

	virtual				~Analysis();

	Q_INVOKABLE	QString	fullHelpPath(QString helpFileName);
	Q_INVOKABLE void	duplicateMe();
	Q_INVOKABLE QString generateWrapper();

	bool				needsRefresh()				const	override;
	bool				wasUpgraded()				const	override	{ return _wasUpgraded; }
	bool				storedWithoutState()		const				{ return _storedWithoutState; }
	bool				isWaitingForModule();
	void				setResults(			const Json::Value & results, analysisResultStatus	status, const Json::Value & progress = Json::nullValue, const std::string & resultsDir = "") { setResults(results, analysisResultsStatusToAnalysisStatus(status), progress, resultsDir); }
	void				setResults(			const Json::Value & results, Status					status, const Json::Value & progress = Json::nullValue, const std::string & resultsDir = "");
	void				imageSaved(			const Json::Value & results);

	void				saveImage(			const Json::Value & options);
	void				editImage(			const Json::Value & options);
	void				imageEdited(		const Json::Value & results);
	void				imagesRewritten(	const Json::Value & results);
	void				rewriteImages();
	bool				isColumnFreeOrMine(const QString & columnName)				const override;
	DataSet		*		dataSet()													const override;

	void				setRFile(const std::string &file)							{ _rfile = file;								}
	void				setRSources(const Json::Value& rSources);
	void				setUserData(Json::Value userData);
	void				setRefreshBlocked(bool block)								{ _refreshBlocked = block;						}
	void				incrementRevision()						{ _revision++;									}
	/// NEO: record the revision that last completed, so the next run can seed from it
	/// (sent as `base_revision`; the orchestrator resolves it to the base results dir for
	/// copy-on-seed incremental recompute). Highest-wins guards against out-of-order results.
	void				setLastCompletedRevision(int rev)		{ if (rev > _lastCompletedRevision) _lastCompletedRevision = rev; }
	int					lastCompletedRevision() const			{ return _lastCompletedRevision;			}

	void				setErrorInResults(const std::string	& msg);

	Json::Value			editOptionsOfPlot(		const std::string & uniqueName, bool emitError = true);
	void				setEditOptionsOfPlot(	const std::string & uniqueName, const Json::Value & editOptions);
	bool				checkAnalysisEntry();

	QVariant			getConstant(const QString& key, const QVariant& defaultValue) const override;
	QVariant			getConstant(const QString& key, const QVariant& defaultValue, const QString& module, const QString& analysis) const override;
	bool				optionLocked(const QString& name) const override;

	const	Json::Value		&	results()			const				{ return _results;							}
	const	Json::Value		&	userData()			const				{ return _userData;							}
	const	std::string		&	name()				const	override	{ return _name;								}
	const	std::string		&	qml()				const				{ return _qml;								}
	const	std::string		&	title()				const	override	{ return _title;							}
	const	std::string		&	titleDefault()		const	override	{ return _titleDefault;						}
	const	std::string		&	rfile()				const				{ return _rfile;							}
	const	std::string			module()			const	override	{ return _moduleData && _moduleData->dynamicModule() ? _moduleData->dynamicModule()->name() : "???";	}
			size_t				id()				const				{ return _id;								}
			Status				status()			const				{ return _status;							}
			QString				statusQ()			const				{ return tq(statusToString(_status));		}
			int					revision()			const				{ return _revision;							}
			std::string			workId()			const		{ return "a" + std::to_string(_id);							}	///< stable work-unit id (§19.1): the analysis instance id. Unique per session via Analyses' id assignment, upholding JaspClient::submit's caller-uniqueness contract.
			const std::string &	datasetId()			const		{ return _datasetId;									}	///< NEO: the dataset this analysis is bound to (data-model-design.md §3.5) — active id at creation, "" for legacy
			bool				isRefreshBlocked()	const				{ return _refreshBlocked;					}
	Q_INVOKABLE	QString			helpFile()			const	override	{ return _helpFile;							}
	const	Json::Value		&	imgOptions()		const				{ return _imgOptions;						}
	const	Json::Value		&	imgResults()		const				{ return _imgResults;						}
	Modules::DynamicModule	*	dynamicModule()		const				{ return _dynamicModule;					}
			AnalysisForm	*	form()				const				{ return _analysisForm;						}
			bool				hasForm()			const				{ return _analysisForm;						}
			bool				isDuplicate()		const	override	{ return _isDuplicate;						}
			bool				isReport()			const				{ return _isReport;						}
			void				setReport(bool report)					{ _isReport = report;							}
			bool				beingTranslated()						{ return _beingTranslated;					};
			void				setBeingTranslated(bool value)			{ _beingTranslated = value;					};
	const	Json::Value		&	resultsMeta()		const	override	{ return _resultsMeta;						}
			void				setTitle(const std::string& title)	override;
			void				run()						override;
			void				refresh()					override;
			void				reloadForm()				override;
			void				exportResults()				override;
			void				remove();
			Json::Value			asJSON(bool withRSources = false)	const;
			void				checkDefaultTitleFromJASPFile(	const Json::Value & analysisData);
			void				loadResultsUserdataAndRSourcesFromJASPFile(const Json::Value & analysisData, Status status);
			Json::Value			createWorkJson();

	static	Status				parseStatus(std::string name);

	bool isEmpty()			const { return status() == Empty;		}
	bool isAborted()		const { return status() == Aborted;		}
	bool isFinished()		const { return status() == Complete || isErrorState(); }
	bool isErrorState()		const { return status() == ValidationError  || status() == FatalError; }

	std::string				qmlFormPath(bool addFileProtocol = true, bool ignoreReadyForUse = false)	const	override;
	void Q_INVOKABLE		createForm(QQuickItem* parentItem = nullptr)										override;

	stringset				usedVariables();
	stringset				createdVariables();
	void					runScriptRequestDone(const QString & result, const QString & controlName, bool hasError);

	void					setUpgradeMsgs(const Modules::UpgradeMsgs & msgs);

	const stringvec &		upgradeMsgsForOption(const std::string & name)		const	override;
	const Version	&		moduleVersion()										const	override	{ return _dynamicModule ? _dynamicModule->version() : AppInfo::version; }

	const Json::Value	&	getRSource(const std::string & name)		const	override	{ return _rSources.count(name) > 0 ? _rSources.at(name) : Json::Value::null; }
	Json::Value				rSources()									const;
	bool					isOwnComputedColumn(const std::string& col)	const	override;
	void					preprocessMarkdownHelp(QString & md)		const				{ if (_dynamicModule) _dynamicModule->preprocessMarkdownHelp(md);}

	
signals:
	void					titleChanged();
	void					needsRefreshChanged();
	void					dynamicModuleChanged();

	void					statusChanged(			Analysis * analysis);
	void					imageSavedSignal(		Analysis * analysis);
	void					imageEditedSignal(		Analysis * analysis);
	void					resultsChangedSignal(	Analysis * analysis);
	void					userDataChangedSignal(	Analysis * analysis);
	void					imageChanged();
	void					rSourceChanged(QString optionName);

	// The excision, Cut 5: requestComputedColumnCreation/requestColumnCreation/
	// requestComputedColumnDestruction signals died with Column (their only purpose was
	// legacy computed-column bookkeeping). The QML-side handlers remain as inert stubs.

	void					refreshTableViewModels();
	void					expandAnalysis();
	void					emptyQMLCache();

	void					createFormWhenYouHaveAMoment(QQuickItem* parent = nullptr);
	void					analysisInitialized();
	void					userModifiedSomething();

	
	
public slots:
	void					setDynamicModule(	Modules::DynamicModule * module);
	void					emitDuplicationSignals();
	void					showDependenciesOnQMLForObject(QString uniqueName); //uniqueName is basically "name" in meta in results.
	void					boundValueChangedHandler()																	override;
	void					requestComputedColumnCreationHandler(	const std::string & columnName)						override;
	void					requestColumnCreationHandler(			const std::string & columnName, columnType colType)	override;
	void					requestComputedColumnDestructionHandler(const std::string & columnName)						override;
	void					analysisQMLFileChanged();
	void					setRSyntaxTextInResult(bool show);
	void					filterByNameDone(int dataSetId, const QString &name, const QString &error);
	void					onUsedVariablesChanged()																	override;
	void					filterRemoved(Filter * f);

protected:
	void					abort();
	void					watchQmlForm();

private:
	void					processResultsForDependenciesToBeShown();
	bool					processResultsForDependenciesToBeShownMetaTraverser(const Json::Value & array);
	bool					_editOptionsOfPlot(const	Json::Value & results, const std::string & uniqueName,			Json::Value & editOptions);
	bool					_setEditOptionsOfPlot(		Json::Value & results, const std::string & uniqueName, const	Json::Value & editOptions);
	void					storeUserDataEtc();
	void					fitOldUserDataEtc();
	bool					updatePlotSize(const std::string & plotName, int width, int height, Json::Value & root);
	void					checkForRSources();
	void					clearRSources();
	void					initAnalysis();
	void					setAnalysisForm(AnalysisForm	* analysisForm);
	bool					readyToCreateForm() const;
	Json::Value				loadPlotlyJsonInResults(Json::Value results) const;

protected:
	Status						_status				= Empty;
	bool						_refreshBlocked		= false;
	Json::Value					_results			= Json::nullValue,
								_resultsMeta		= Json::nullValue,
								_imgResults			= Json::nullValue,
								_userData			= Json::nullValue,
								_imgOptions			= Json::nullValue,
								_progress			= Json::nullValue,
								_oldUserData		= Json::nullValue,
								_oldMetaData		= Json::nullValue;
	std::string					_preUpgraderVersion	= "0";

	// NEO: orchestrator's per-revision dir holding this analysis' file artifacts (plot PNGs +
	// plotly JSON) — wired in with each result. Asset paths in `_results` stay RELATIVE (that is
	// what lands on disk); the dir is used only to rewrite them for the results webview.
	std::string					_resultsDir;

	// NEO (data-model-design.md §3.5): the dataset this analysis works on — bound at creation
	// from the registry's active dataset, so an analysis keeps its dataset when the active one
	// changes. "" for legacy/.jasp-loaded analyses (identical to pre-binding behavior).
	std::string					_datasetId;


private:
	size_t						_id,
								_counter						= 0;
	std::string					_name,
								_qml,
								_titleDefault,
								_title,
								_rfile,
								_showDepsName					= "",
								_codedReferenceToAnalysisEntry	= "",
								_lastQmlFormPath				= "";
	bool						_isDuplicate					= false,
								_wasUpgraded					= false,
								_optionsFromDifferentVersion	= false,
								_storedWithoutState				= false,
								_tryToFixNotes					= false,

								_hasReport					= false,
								_beingTranslated			= false,
								_isReport				= false;
	Json::Value					_lastSentMeta				= Json::nullValue;
	int							_revision						= 0;
	int							_lastCompletedRevision			= -1;	///< NEO: highest revision that completed (-1 = none yet); sent as base_revision

	Modules::AnalysisEntry	*	_moduleData						= nullptr;
	Modules::DynamicModule	*	_dynamicModule					= nullptr;
	QFileSystemWatcher			_QMLFileWatcher;
	QString						_helpFile;
	Modules::UpgradeMsgs		_msgs;
	std::map<std::string,
	Json::Value>				_rSources;

};

#endif // ANALYSIS_H
