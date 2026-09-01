#include <QTest>
#include <QTemporaryDir>

class DataSetPackage;
class DataSet;
namespace Json { class Value; }	///< moc-compiles standalone — the fixture below only references it

class TestAll: public QObject
{
    Q_OBJECT

private slots:
    void    initTestCase();
    void    init();
	void	cleanup();

	// The excision, Cut 2: the importer/exporter test blocks (testDataImport,
	// testJaspDataImport, testJaspRoundRobin, testSavLabels, testFilterLabels,
	// testDataExporterShownDataSetOnly) died with Desktop/data/importers and
	// Desktop/data/exporters — those formats return as lane conversions in later
	// NEO eras (refactor_design/HANDOVER-excision.md).

	// The excision, Cut 3: testFilterRevisionInvalidatedRoundTrip died with DatabaseInterface
	// (it pinned a sqlite filterLoad round-trip; filters are in-memory now).

	// The excision, Cut 6: testFilterSetFilterVectorResizesToResult died with Filter's per-row
	// mask — filters return as derived boolean columns.

	// Computed-dataset cycle prevention: a computed dataset must not depend on a dataset that
	// (transitively) depends on it, or the recompute cascade would livelock.
	void	testComputedDataSetCycleDetection();

	// Undo regression: testUndoColumnDropLevels died in Cut 2 (its fixture was the importer-built
	// legacy Column; the Column undo-command family is Cut-4 death row anyway).

	// Encoder regression: each dataset's encoder prefix must carry the dataset id (not -1), so
	// colliding column names across datasets cannot encode to the same name.
	void	testEncoderPrefixPerDataset();

	// Filter ownership: removeFilter must unregister (no dangling pointer in _filters) and
	// runFilters() must stay safe afterwards.
	void	testFilterRemoveFilter();

	// Closing/removing datasets and workspaces must never crash (regression for the dataset-close crash
	// and the workspace teardown paths).
	void	testCloseWorkspaceAndDataSets();

	// Lane data_changed / applyRevision scenarios (data-edit-design §6, Increment 4 step (c)):
	// the DataSet-side contract of every edit's return leg — revision adoption, rows/schema
	// landing, the view-restart signal, and the staleness/idempotence guards. The GridModel
	// restart itself is wired in Desktop (bindToShown) and covered by the Rust e2e rail; here
	// we pin what applyRevision promises its callers.
	void	testLaneRevisionLandsRowsAndSchema();
	void	testLaneRevisionIgnoresStalePushes();
	void	testLaneRevisionSchemaSwap();
	void	testLaneRevisionRowGrowthWithoutSchema();
	void	testLaneRevisionOutOfOrderPushes();
	void	testLaneDatasetsHaveNoMirrorColumns();	///< the R2 canary: lane ⇒ zero legacy Columns
	void	testLaneColumnModelServesSchema();	///< the R2 canary 2: the variable editor's model serves the schema on lane

	private:
		DataSetPackage		*	_pkg	= nullptr;
		bool					_newPkgWithDataSet();
		/// A lane-bound DataSet WITHOUT any import: `applySchema` plays the open result (the
		/// orchestrator's kind:"data" terminal), revision space starting at 0 — the minimal
		/// fixture for the applyRevision scenarios above.
		DataSet				*	_newLaneDataSet(const Json::Value & schema, uint64_t rows);
};
