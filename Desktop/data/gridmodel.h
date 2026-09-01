#ifndef GRIDMODEL_H
#define GRIDMODEL_H

#include <QAbstractTableModel>

#include "dataviewbuffer.h"
#include "dataenums.h"
#include "columntype.h"
#include "columninfo.h"

class DataSet;
class ViewFiller;

/// NEO grid model — the direct `dataSetModel` (refactor_design/data-view-design.md §7.2/§7.3,
/// the burn-down seam: NO switcher façade, NO legacy fallback).
///
/// Multi-dataset fold: binds to the SHOWN DataSet (Workspace is the one dataset truth —
/// the registry is gone). Serves from the shown dataset's schema (identity + metadata
/// live on DataSet now); the view lane (DataViewBuffer + ViewFiller) is GridModel-owned and
/// recreated per shown dataset — the same drop-on-switch memory policy the registry had
/// (data-view-design §7.6: only the shown dataset holds a buffer, ceiling = 1 × budget).
/// Rows span the whole dataset — the view's item creation is viewport-driven, so a 30M-row
/// span is as cheap as a 30-row one. Non-resident cells render as placeholders until their
/// chunk arrives — sliding mode, format doc §2.5. Read-only in v1: `flags()` never sets
/// ItemIsEditable and `setData` refuses — the editing surface returns with `data_edit`.
class GridModel : public QAbstractTableModel
{
	Q_OBJECT

	// The excision, Cut 7: the columnsFilteredCount Q_PROPERTY died with the label editor QML
	// (v1 has no filters; the signal was never emitted).
	Q_PROPERTY(bool showInactive			READ showInactive	WRITE setShowInactive	    NOTIFY showInactiveChanged	     	 )
	/// Status line for the grid's status bar: "" while the fill is in progress or complete;
	/// a note when the fill stopped early (buffer budget exhausted / lane failure).
	Q_PROPERTY(QString viewStatus		READ viewStatus							NOTIFY viewStatusChanged		)
	/// Column-name filter typed in the status bar's "Columnfilter" box (multi-dataset PR QML).
	/// Stored + notified; actual column filtering is edit-era (the header view must learn to skip
	/// non-matching columns — until then setting it logs once so the no-op is visible, not silent.
	Q_PROPERTY(QString columnFilter	READ columnFilter	WRITE setColumnFilter	NOTIFY columnFilterChanged		)
	/// Type icons of the currently selected columns (status-bar type toggle row). Selection is
	/// view-side state GridModel does not track — stays empty until the edit era wires it.
	Q_PROPERTY(QVariantList currentTypeIcons	READ currentTypeIcons				NOTIFY currentTypeIconsChanged	)

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

	// QML-invokable surface QML calls on `dataSetModel` (reads → the shown DataSet's lane schema; mutations no-op in v1)
	Q_INVOKABLE QString		columnName(int column) const;
	Q_INVOKABLE void		setColumnName(int col, QString name);			///< no-op until data_edit
	Q_INVOKABLE QVariant	getColumnTypesWithIcons() const;
	Q_INVOKABLE QVariant	columnTypesWithIcons() const { return getColumnTypesWithIcons(); }///< multi-dataset PR QML renamed the call (no "get")
	Q_INVOKABLE bool		columnUsedInEasyFilter(int column) const;		///< false until filters
	Q_INVOKABLE void		resetAllFilters();					///< no-op until filters
	// The excision, Cut 7: isColumnNameFree died with CreateComputeColumnDialog (DataSet's
	// schema predicate stays for the rename flows that return).
	Q_INVOKABLE void		toggleColType(int column, bool next = true);	///< edit-era: logged no-op (fail loudly, merge-multidataset.md §6)

	int					columnsFilteredCount() const { return 0; }		///< no filters in v1
	/// The SHOWN dataset — identity + schema holder (nullptr = nothing shown). The edit
	/// surface (the proxy's commands) needs it for datasetId/laneRevision at submit time.
	DataSet			*	dataSet() const { return _dataSet; }
	/// The grid's current viewport row range [firstRow, lastRow) — forwarded to the shown
	/// dataset's fill scheduler (MainWindow wires DataSetViewBase::viewportRowsChanged here).
	/// No-op without a live lane.
	void				setViewportRows(uint64_t firstRow, uint64_t lastRow);
	QString				columnFilter() const { return _columnFilter; }
	void				setColumnFilter(const QString & filter);
	QVariantList		currentTypeIcons() const { return QVariantList(); }	///< empty until selection is wired (edit era)
	bool				showInactive() const { return _showInactive; }
	void				setShowInactive(bool showInactive);
	QString				viewStatus() const { return _viewStatus; }

signals:
	void				columnsFilteredCountChanged();
	void				showInactiveChanged(bool showInactive);
	void				viewStatusChanged(const QString & status);
	void				columnFilterChanged(const QString & filter);
	void				currentTypeIconsChanged();
	void				renameColumnDialog(int columnIndex);			///< RenameColumnDialog listens; never emitted in v1

private slots:
	void				bindToShown();			///< Workspace::shownDataSetChanged — rebind the whole lane
	void				onLaneSchemaChanged();	///< shown dataset's applySchema landed (open completed) — (re)start its lane
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
	static qreal	columnWidthFallbackFor(columnType type);
	void				startView();			///< fresh buffer + filler for the shown lane dataset (rows > 0)
	void				dropView();			///< stop the filler + drop the buffer (memory policy §7.6)

	DataSet			*	_dataSet		= nullptr;	///< the SHOWN dataset (identity + lane schema; nullptr = nothing shown)
	DataViewBuffer	*	_buffer			= nullptr;	///< the shown dataset's resident cells (GridModel-owned, dropped on switch)
	ViewFiller		*	_filler			= nullptr;	///< its chunked fill driver (GridModel-owned, dropped on switch)
	uint64_t			_viewEpoch		= 0;		///< bumped per fill — the buffer's fill identity
	bool				_showInactive	= true;
	QString				_viewStatus;	///< grid status-bar note (windowed-mode note / fill failure)
	QString				_columnFilter;	///< status-bar column filter text (stored; filtering is edit-era)

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
