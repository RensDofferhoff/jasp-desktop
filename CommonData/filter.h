#ifndef FILTER_H
#define FILTER_H

#include "datasetbasenode.h"
#include <string>
#include <vector>
#include "utils.h"

#define DEFAULT_FILTER_JSON	"{\"formulas\":[]}"
#define DEFAULT_FILTER_GEN	"generatedFilter <- rep(TRUE, rowcount)"
#define DEFAULT_FILTER_NAME "DEFAULT_FILTER"

class DataSet;
// The excision, Cut 5: the LabelFilterGenerator fwd-decl died with the class (B2).
// The excision, Cut 3: the DatabaseInterface forward declaration is gone with the class.
// The excision, Cut 6: Filter is no longer a VariableInfoProvider and carries no per-row
// mask (FilteredData/VarInfoModelProxy/_filtered died) — the forms are served by the
// Workspace-injected provider (ColumnsModel/DataSetProvider), the schema is the truth.
// Filters return in a later NEO era as DERIVED BOOLEAN COLUMNS: the frontend keeps only
// the expression (constructorJson/rFilter — metadata, zero rows); the backend evaluates it
// and the VIEW LANE applies the mask when serving chunks. No per-row vector ever exists
// in the frontend (HANDOVER-excision.md).

/// A named or (as the default filter) unnamed filter expression owned by a DataSet.
///
/// It stores the R-filter constructor expression and its errormsgs — metadata only.
/// If a filter has a name it is used by an analysis only, if not it is part of the
/// DataSet and coupled with the GUI.
class Filter : public DataSetBaseNode
{
	Q_OBJECT

	friend DataSet;

	Q_PROPERTY( QString			name				READ nameQ											NOTIFY nameChanged				)
	Q_PROPERTY( QString			generatedFilter		READ generatedFilterQ	WRITE setGeneratedFilterQ	NOTIFY generatedFilterChanged	)
	Q_PROPERTY( QString			rFilter				READ rFilterQ			WRITE setRFilterQ			NOTIFY rFilterChanged			)
	Q_PROPERTY( QString			constructorJson		READ constructorJsonQ	WRITE setConstructorJsonQ	NOTIFY constructorJsonChanged	)
	Q_PROPERTY( QString			constructorR		READ constructorRQ		WRITE setConstructorRQ		NOTIFY constructorRChanged		)
	Q_PROPERTY( QString			statusBarText		READ statusBarText									NOTIFY statusBarTextChanged		)
	Q_PROPERTY( QString			filterErrorMsg		READ filterErrorMsgQ								NOTIFY filterErrorMsgChanged	)
	Q_PROPERTY( bool			hasFilter			READ hasFilter										NOTIFY hasFilterChanged			)
	Q_PROPERTY( QString			defaultRFilter		READ defaultRFilter									NOTIFY defaultRFilterChanged	)
	Q_PROPERTY( bool			invalidated			READ invalidated									NOTIFY invalidatedChanged		)

public:
	DataSet					*	data()				const { return _data;					}
	int							id()				const { return _id;						}
	const std::string		&	name()				const { return _name;					}
	bool						isDataSetFilter()	const { return _name.empty();			} ///< If the Filter has a name it is created by an analysis or something. Otherwise it represents a (possible) combination of a drag'n'drop filter, labels-filter and/or R-filter as manually entered in the GUI
	const std::string		&	rFilter()			const { return _rFilter;				}
	const std::string		&	generatedFilter()	const;
	const std::string		&	constructorJson()	const { return _constructorJson;		}
	const std::string		&	constructorR()		const { return _constructorR;			}
	bool						invalidated()		const { return _invalidated;			}
	const std::string		&	errorMsg()			const { return _errorMsg;				}

	QString						nameQ()					const;
	QString						title()					const { return _name == DEFAULT_FILTER_NAME ? QObject::tr("Default filter") : nameQ(); };
	QString						rFilterQ()				const;
	QString						constructorRQ()			const;
	QString						statusBarText()			const	{ return _statusBarText;			}
	QString						filterErrorMsgQ()		const;
	QString						generatedFilterQ()		const;
	QString						constructorJsonQ()		const;

	// The excision, Cut 3: the db* family (dbCreate/dbUpdate/dbUpdateErrorMsg/dbLoad/
	// dbLoadResultAndError/dbDelete/db()) died with DatabaseInterface — the id is minted in
	// the ctor from a process-global counter; setters bump the revision inline.
	void					incRevision() override;

	bool						columnUsed(const QString & name) const;

	static	const QString	&	defaultRFilter();

	bool						hasFilter()				const;

	void						setRFilterQ(			const QString & newRFilter			);
	void						setConstructorRQ(		const QString & newConstructorR		);
	void						setGeneratedFilterQ(	const QString & newGeneratedFilter	);
	void						setConstructorJsonQ(	const QString & newconstructorJson	);
	void						setFilterErrorMsgQ(		const QString & newFilterErrorMsg	);
	void						setStatusBarText(		const QString & newStatusBarText	);

	void						setRFilter(			const std::string	& rFilter);
	void						setGeneratedFilter(	const std::string	& generatedFilter);
	void						setConstructorJson(	const std::string	& constructorJson);
	void						setConstructorR(	const std::string	& constructorR);
	void						setInvalidated(		bool			 	  invalidated);
	void						setErrorMsg(		const std::string	& errorMsg);
	void						setName(			const std::string	& name);
	void						setId(				int		id)			{ _id = id; }

	stringset				columnsUsedInConstructor()	const;

	stringset					columnsUsedInRFilter()		const;

	static	bool				filterNameIsFree(const std::string & filterName, DataSet * dataSet);

	void					reset();

signals:
	void						nameChanged();
	void						rFilterChanged();
	void						hasFilterChanged();
	void						invalidatedChanged();
	void						refreshAllAnalyses(Filter * f);
	void						refreshAllCompCols(Filter * f);
	void						dataSetShouldRefresh(bool doColumnsToo=true);
	void						constructorRChanged();
	void						statusBarTextChanged();
	void						filterErrorMsgChanged();
	void						defaultRFilterChanged(); //should we cll this on a language change? Or does it automatically go right cause we reset qml?
	void						generatedFilterChanged();
	void						constructorJsonChanged();

protected:
	void						rescanForColumns();
	void						connectionCreation();

protected slots:
	void						datasetChanged(int dataSetId, QStringList changedColumns, QStringList missingColumns, QMap<QString, QString> changeNameColumns, bool rowCountChanged, bool);


private:
	Filter(DataSet * data);
	Filter(DataSet * data, const std::string & name, bool createIfMissing = true);

	DataSet					*	_data				= nullptr;
	int							_id					= -1;
	std::string					_rFilter			= "",
								_generatedFilter	= "",
								_constructorJson	= "",
								_constructorR		= "",
								_errorMsg			= "",
								_name				= "";
	bool						_invalidated		= false;
	stringset					_columnsInConstructorJson,
								_columnsUsedInRFilter;
	QString						_statusBarText;
};

typedef std::vector<Filter*>	Filters;
typedef std::set<Filter*>		FilterSet;

#endif // FILTER_H
