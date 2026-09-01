#ifndef COLUMNSMODEL_H
#define COLUMNSMODEL_H

#include <QAbstractTableModel>
#include "variableinfo.h"
#include "columninfo.h"
#include "models/terms.h"

class DataSet;
class ColumnEncoder;

/// 
/// Model used by the filter-drag-n-drop to give all the columns and their datatypes
/// The columns are layed out as rows to facilitate that
class ColumnsModel  : public QAbstractTableModel, public VariableInfoProvider
{
	Q_OBJECT
public:
	enum ColumnsModelRoles {
		NameRole = Qt::UserRole + 1,
		TypeRole,
		ColumnTypeRole,
		ComputedColumnTypeRole,
		IconSourceRole,
		ToolTipRole
	 };
											ColumnsModel();
											~ColumnsModel()		override;

				QVariant					data(			const QModelIndex & index, int role = Qt::DisplayRole)				const	override;
				int							rowCount(		const QModelIndex &parent = QModelIndex())							const	override;
				QHash<int, QByteArray>		roleNames()																			const	override;
				int							getColumnIndex(const std::string & col)											const	{ return _laneDataSet && _laneDataSet->isOpen() ? _laneDataSet->schemaColumnIndex(col) : -1;	}
				void						bindLane(DataSet * dataSet);	///< multi-dataset fold: serve the SHOWN dataset; when orchestrator-backed the wire schema is the source of truth (data-model-design.md §3.4)
				int						columnCount(	const QModelIndex &parent = QModelIndex())								const	override;
				QStringList					getColumnNames()																							const;
				const Terms &				dataSetTerms()																									const;	///< wide-data: cached (name, type) Terms of the active dataset, rebuilt only when the columns change
	Q_INVOKABLE	int							getColumnType(const QString & name)													const;
	Q_INVOKABLE	QString						getColumnIcon(int columnType)														const;
	Q_INVOKABLE	QString						getColumnIcon(int columnType, bool isTransformed)									const;
				QString						getColumnIcon(columnType colType)													const;
	Q_INVOKABLE QString						getColumnDescription(const QString & name)											const;
	Q_INVOKABLE	QString						getColumnIconTransform(int columnType)												const;
				QString						getColumnIconTransform(columnType colType)											const;
				QString						getColumnTransformedToolTip(const QString & name, columnType transformedTo)			const;
	Q_INVOKABLE	QString						getColumnTransformedToolTip(const QString & name, int transformedTo)				const;

				QVariant				provideInfo(varInfoType info, const QString& colName = "", int row = 0)		const	override;
				bool				absorbInfo(	varInfoType info, const QString& name, int row, QVariant value)			override;
				QAbstractItemModel	*	providerModel()																				override	{ return this;	}
				/// The excision, Cut 6: serve the bound dataset's own encoder (JAGS/R-syntax text
				/// areas en-/decode against it instead of the process-global fallback).
				ColumnEncoder		*	columnEncoder()																				override;

	static		ColumnsModel			*	singleton()	{ return _singleton; }

public slots:
	void datasetChanged(int dataSetId, QStringList changedColumns, QStringList missingColumns, QMap<QString, QString> changeNameColumns, bool rowCountChanged, bool hasNewColumns);
	void refresh() { beginResetModel(); endResetModel(); }

signals:
	void columnNamesChanged(QMap<QString, QString>	changedNames);
	void columnsChanged(	QStringList				changedColumns);
	void columnTypeChanged(	QString					colName);
	void labelsChanged(		QString					columnName, QMap<QString, QString> changedLabels);
	void labelsReordered(	QString					columnName);
	void filterChanged();
	void dataSetChanged();

private:
	DataSet					*	_laneDataSet	= nullptr;	///< the SHOWN dataset (multi-dataset fold); when orchestrator-backed (datasetId set) the wire schema is the source of truth
	static ColumnsModel		* _singleton;

	// Wide-data cache (2026-08-16): the dataset's Terms built once per column-set change and
	// handed out via VariableInfo::DataSetTerms — replaces k per-name requestInfo roundtrips
	// per sourceTermsReset pass. Mutable: provideInfo() is const.
	mutable Terms				_dataSetTermsCache;
	mutable bool				_dataSetTermsValid = false;
};



#endif // COLUMNSMODEL_H
