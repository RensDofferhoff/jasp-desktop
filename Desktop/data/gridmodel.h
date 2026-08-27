#ifndef GRIDMODEL_H
#define GRIDMODEL_H

#include <QAbstractTableModel>

#include "dataviewbuffer.h"
#include "datasetpackageenums.h"
#include "columntype.h"

class DataModel;
class DatasetRegistry;
class ViewFiller;

/// NEO grid model — the direct `dataSetModel` (refactor_design/data-view-design.md §7.2/§7.3,
/// the burn-down seam: NO switcher façade, NO legacy fallback).
///
/// Rows come from the active DataModel's schema row count (the model ALWAYS spans the whole
/// dataset — the view's item creation is viewport-driven, so a 30M-row span is as cheap as a
/// 30-row one), cells from the registry-owned DataViewBuffer. Rows whose chunk is not resident
/// (not yet filled, or evicted to make room) render as placeholder cells until their chunk
/// arrives — sliding mode, format doc §2.5. Read-only in v1: `flags()` never sets ItemIsEditable
/// and `setData` refuses — the editing surface returns with `data_edit`.
///
/// The QML invokables QML calls on `dataSetModel` are served from the DataModel (reads) or
/// no-op'd (mutations). Binds to the registry's active dataset + its view buffer; a dataset
/// switch/reset rebinds under a model reset.
class GridModel : public QAbstractTableModel
{
	Q_OBJECT

	Q_PROPERTY(int	columnsFilteredCount	READ columnsFilteredCount					NOTIFY columnsFilteredCountChanged	)
	Q_PROPERTY(bool showInactive			READ showInactive	WRITE setShowInactive	NOTIFY showInactiveChanged			)
	/// Status line for the grid's status bar: "" while the fill is in progress or complete;
	/// a note when the fill stopped early (buffer budget exhausted / lane failure).
	Q_PROPERTY(QString viewStatus			READ viewStatus							NOTIFY viewStatusChanged			)

public:
	explicit GridModel(QObject * parent = nullptr);

	// QAbstractItemModel
	int					rowCount(		const QModelIndex & parent = QModelIndex())						const	override;
	int					columnCount(	const QModelIndex & parent = QModelIndex())						const	override;
	QVariant			data(			const QModelIndex & index, int role = Qt::DisplayRole)			const	override;
	QVariant			headerData(		int section, Qt::Orientation orientation, int role = Qt::DisplayRole)	const	override;
	Qt::ItemFlags		flags(			const QModelIndex & index)										const	override;
	QHash<int, QByteArray> roleNames()																	const	override;
	bool				setData(		const QModelIndex & index, const QVariant & value, int role)			override;

	// QML-invokable surface QML calls on `dataSetModel` (reads → DataModel; mutations no-op in v1)
	Q_INVOKABLE QString		columnName(int column) const;
	Q_INVOKABLE void		setColumnName(int col, QString name);			///< no-op until data_edit
	Q_INVOKABLE QVariant	getColumnTypesWithIcons() const;
	Q_INVOKABLE bool		columnUsedInEasyFilter(int column) const;		///< false until filters
	Q_INVOKABLE void		resetAllFilters();							///< no-op until filters
	Q_INVOKABLE bool		isColumnNameFree(QString name) const;

	int					columnsFilteredCount() const { return 0; }		///< no filters in v1
	bool				showInactive() const { return _showInactive; }
	void				setShowInactive(bool showInactive);
	QString				viewStatus() const { return _viewStatus; }

signals:
	void				columnsFilteredCountChanged();
	void				showInactiveChanged(bool showInactive);
	void				viewStatusChanged(const QString & status);
	void				renameColumnDialog(int columnIndex);			///< RenameColumnDialog listens; never emitted in v1

private slots:
	void				bindToActive();
	void				onChunkIngested(quint64 firstRow, quint64 rows);
	void				onChunksEvicted(quint64 firstRow, quint64 rows);
	void				onBufferReset();
	void				onFillBudgetReached(quint64 rowsResident, quint64 rowsTotal);
	void				onFillCompleted();
	void				onFillFailed(QString message);
	void				onFillRecovered();	///< data flowed again after a failure — drop the error note

private:
	void				refreshRows(quint64 firstRow, quint64 rows);	///< dataChanged over a chunk's rows (content changed; rows always exist)
	void				ensureCell(int row, int col) const;
	static qreal		columnWidthFallbackFor(columnType type);

	DatasetRegistry	*	_registry		= nullptr;
	DataModel		*	_model			= nullptr;	///< active dataset's schema (nullable)
	DataViewBuffer	*	_buffer			= nullptr;	///< active dataset's resident cells (nullable)
	ViewFiller		*	_filler			= nullptr;	///< active dataset's fill driver (nullable)
	bool				_showInactive	= true;
	QString				_viewStatus;	///< grid status-bar note (windowed-mode note / fill failure)

	// Per-cell role fan-out cache: the grid's viewport loop asks ~6 roles per cell,
	// column-major — compute the cell once and reuse it across the role calls.
	mutable uint64_t	_cacheRow		= UINT64_MAX;
	mutable int			_cacheCol		= -1;
	mutable QString		_cacheText;
	mutable bool		_cacheNull		= false;
	mutable bool		_cacheMissing	= false;	///< row's chunk not resident → placeholder rendering
	static const QString	placeholderText;	///< non-resident cell marker (distinct from empty/null cells)
};

#endif // GRIDMODEL_H
