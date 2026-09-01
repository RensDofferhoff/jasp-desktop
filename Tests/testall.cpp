#include "testall.h"
#include "testinfo.h"
#include "tempfiles.h"
#include "processinfo.h"
#include "qutils.h"
#include "databaseinterface.h"
#include "data/datasetpackage.h"
#include "utilities/settings.h"
#include "dataset.h"
#include "workspace.h"
#include "undostack.h"
#include "data/columnmodel.h"

#include <QSignalSpy>
#include <QFile>
#include <QFileInfo>
#include <sqlite3.h>
#include "data/asyncloader.h"

// ---------- Lane fixture builders (data-edit-design §6 / data-model-design §3.2) ----------
// Wire-schema builders — the lane's column-info JSON exactly as column_info_json emits it
// (name / type / levels). The excision, Cut 2: these also replaced the CSVImporter fixture
// loading — `applySchema` is how a test makes a dataset non-empty now (zero legacy Columns).
static Json::Value laneColumn(const std::string & name, const std::string & type, std::initializer_list<const char *> levels = {})
{
	Json::Value col;
	col["name"]	= name;
	col["type"]	= type;
	if (levels.size())
	{
		col["levels"] = Json::Value(Json::arrayValue);
		for (const char * level : levels)
			col["levels"].append(level);
	}
	return col;
}

static Json::Value laneSchema(std::initializer_list<Json::Value> columns)
{
	Json::Value schema(Json::arrayValue);
	for (const Json::Value & col : columns)
		schema.append(col);
	return schema;
}

///The minimal "non-empty dataset" fixture: one scale column, three rows.
static Json::Value fixtureSchema()
{
	return laneSchema({ laneColumn("V1", "scale") });
}

void TestAll::initTestCase()
{
	TempFiles::init(ProcessInfo::currentPID()); // needed here so that the LRNAM can be passed the session directory
}

void TestAll::init()
{
	Settings::informSettingsThatThisIsATest();
}

void TestAll::cleanup()
{
	DatabaseInterface::singleton()->close();
	DatabaseInterface::singleton()->closeInterfaces();
	delete _pkg;
	_pkg = nullptr;
}

bool TestAll::_newPkgWithDataSet()
{
	delete _pkg;
	_pkg = nullptr;

	_pkg = new DataSetPackage(this);

	//The excision, Cut 2: the CSVImporter fixture died with the importers; applySchema plays
	//a non-empty dataset instead — same shape (one column, some rows), zero legacy Columns.
	DataSet * dataSet = _pkg->createDataSet();
	dataSet->applySchema("ds-test-fixture", 3, fixtureSchema(), "");

	return _pkg->dataSet() != nullptr;
}

#define TO_STR2(x) #x
#define TO_STR(x) TO_STR2(x)

// The excision, Cut 2: testDataImport/_data, testJaspDataImport/_data, testJaspRoundRoundRobin/_data,
// testSavLabels, testFilterLabels and testDataExporterShownDataSetOnly died with
// Desktop/data/importers and Desktop/data/exporters. Those formats return as lane
// conversions in later NEO eras (refactor_design/HANDOVER-excision.md).

void TestAll::testFilterSetFilterVectorResizesToResult()
{
	QVERIFY(_newPkgWithDataSet());

	DataSet * ds = _pkg->dataSet();
	QVERIFY(ds);
	QVERIFY(ds->rowCount() > 0);

	Filter * filter = ds->defaultFilter();
	QVERIFY(filter);

	const size_t originalRows = static_cast<size_t>(ds->rowCount());

	//Seed a cache matching the current dataset.
	boolvec initial(originalRows, true);
	initial[0] = false;
	filter->setFilterVector(initial);
	QCOMPARE(filter->filtered().size(), originalRows);

	//The dataset grew: the engine result is authoritative and must be adopted in full (new rows at
	//the end get the engine's value), instead of silently dropping everything past the old size.
	boolvec bigger(originalRows + 3, false);
	bigger[0] = false, bigger[1] = true, bigger[bigger.size() - 1] = true;
	filter->setFilterVector(bigger);
	QCOMPARE(filter->filtered().size(), originalRows + 3);
	QVERIFY(filter->filtered() == bigger);

	//And when the result shrinks, stale tail rows must not survive.
	boolvec smaller(originalRows - 2, true);
	filter->setFilterVector(smaller);
	QCOMPARE(filter->filtered().size(), originalRows - 2);
	QVERIFY(filter->filtered() == smaller);
}

void TestAll::testComputedDataSetCycleDetection()
{
	QVERIFY(_newPkgWithDataSet());

	Workspace * ws = _pkg->workspace();
	QVERIFY(ws);

	//Workspace::createDataSet reuses the currently-shown (empty) dataset, so make each one
	//non-empty before creating the next, to get three distinct datasets. The excision, Cut 2:
	//the CSVImporter fixture is gone — applySchema plays a non-empty dataset instead.
	DataSet * a = ws->createDataSet();
	QVERIFY(a);
	a->applySchema("ds-test-cycle-a", 3, fixtureSchema(), "");
	DataSet * b = ws->createDataSet();
	QVERIFY(b);
	b->applySchema("ds-test-cycle-b", 3, fixtureSchema(), "");
	DataSet * c = ws->createDataSet();
	QVERIFY(c);
	c->applySchema("ds-test-cycle-c", 3, fixtureSchema(), "");

	QVERIFY(a->id() != b->id());
	QVERIFY(b->id() != c->id());
	QVERIFY(a->id() != c->id());

	a->setCodeType(computedColumnType::rCode);
	b->setCodeType(computedColumnType::rCode);
	c->setCodeType(computedColumnType::rCode);

	std::string err;
	QVERIFY(!ws->computedDataSetsHaveLoop(err));

	//A valid chain c -> b -> a is accepted and is not a loop.
	QVERIFY(c->setDefaultInputFilterId(b->defaultFilter()->id()));
	QVERIFY(b->setDefaultInputFilterId(a->defaultFilter()->id()));
	QCOMPARE(c->defaultInputFilterId(), b->defaultFilter()->id());
	QCOMPARE(b->defaultInputFilterId(), a->defaultFilter()->id());
	QVERIFY(!ws->computedDataSetsHaveLoop(err));

	//A depending on C would close the chain into a loop (A <- C <- B <- A) and must be refused,
	//leaving A without an input (the value is unchanged).
	QVERIFY(!a->setDefaultInputFilterId(c->defaultFilter()->id()));
	QCOMPARE(a->defaultInputFilterId(), -1);

	//Likewise A depending on B while B depends on A is a loop and must be refused.
	QVERIFY(!a->setDefaultInputFilterId(b->defaultFilter()->id()));
	QCOMPARE(a->defaultInputFilterId(), -1);

	QVERIFY(!ws->computedDataSetsHaveLoop(err));
}

// testUndoColumnDropLevels died in Cut 2 as well: its fixture was the importer-built legacy
// Column with labels, and its code-under-test (the Column undo-command family) is Cut-4
// death row — there is no honest way to build that Column anymore.

void TestAll::testEncoderPrefixPerDataset()
{
	QVERIFY(_newPkgWithDataSet());

	DataSet * a = _pkg->dataSet();
	QVERIFY(a);
	QVERIFY(a->columnCount() > 0);

	//The excision, Cut 2: the fixture is lane-bound (schema, no legacy Columns), so the
	//column name comes from the schema-served getter.
	QVERIFY(!a->getColumnNames().empty());
	const std::string colName	= a->getColumnNames()[0];
	const std::string prefixA	= "JASPColumn_" + std::to_string(a->id()) + "_";
	const std::string encodedA	= a->encoder().encode(colName);

	QVERIFY2(encodedA.find(prefixA) == 0,	qPrintable("Encoder prefix must carry the dataset id"));
	QVERIFY2(encodedA.find("-1") == std::string::npos,	qPrintable("Encoder prefix must not be the -1 sentinel"));

	//The db-reload (the old .jasp restore path) check died in Cut 2 with the importers: lane
	//schema columns are not written to sqlite, so a dbLoad of this id has no names to encode.
	//Prefix persistence across a real reload returns when .jasp persistence does (a later era).

	//A second dataset with a colliding column name must get a distinct prefix.
	DataSet * b = _pkg->workspace()->createDataSet();
	QVERIFY(b);
	b->applySchema("ds-test-encoder-b", 3, fixtureSchema(), "");
	QVERIFY(b->id() != a->id());

	const std::string prefixB	= "JASPColumn_" + std::to_string(b->id()) + "_";
	const std::string encodedB	= b->encoder().encode(b->getColumnNames()[0]);
	QVERIFY2(encodedB.find(prefixB) == 0,	qPrintable("Second dataset must get its own id-based prefix"));
	QVERIFY(encodedB != encodedA);

	//Instance-level JSON decode must work against the dataset's own encoder (the static
	//ColumnEncoder::decodeJson is a no-op on the desktop: the global current-encoder indirection is
	//only set inside the engine).
	Json::Value json;
	json["axis"] = encodedA;
	a->encoder().decodeJson(json);
	QCOMPARE(json["axis"].asString(), colName);
}

void TestAll::testFilterRemoveFilter()
{
	QVERIFY(_newPkgWithDataSet());

	DataSet * ds = _pkg->dataSet();
	QVERIFY(ds);

	const size_t before = ds->filters().size();

	Filter * f = ds->createFilter("testRemoveMe", true);
	QVERIFY(f);
	QCOMPARE(ds->filters().size(), before + 1);
	QVERIFY(ds->filter("testRemoveMe") == f);

	ds->runFilters(); //must be safe while the filter is present

	//The default filter is not removable and must be a no-op.
	ds->removeFilter(ds->defaultFilter());
	QCOMPARE(ds->filters().size(), before + 1);

	ds->removeFilter(f);
	QCOMPARE(ds->filters().size(), before);
	QVERIFY(ds->filter("testRemoveMe") == nullptr);

	ds->runFilters(); //must still be safe after removal (the dangling-pointer regression would crash here)
}

void TestAll::testFilterRevisionInvalidatedRoundTrip()
{
	//Regression test: filterLoad used to assign `revision` twice (overwriting it with the
	//`invalidated` column) and never loaded `invalidated`. Save a filter and check every
	//field round-trips, especially revision vs invalidated.
	QVERIFY(_newPkgWithDataSet());
	DataSet * ds = _pkg->dataSet();
	QVERIFY(ds);
	DatabaseInterface & dbi = ds->db();
	const int dataSetId = ds->id();
	QVERIFY(dataSetId > 0);

	const std::string originalRFilter		= "filterResult <- x > 1";
	const std::string originalGenerated		= "generated <- TRUE";
	const std::string originalConstructor	= "{\"formulas\":[]}";
	const std::string originalConstructorR	= "constrR <- 1 + 1";
	const std::string originalName			= "roundtripFilter";

	const int filterId = dbi.filterInsert(dataSetId, originalRFilter, originalGenerated, originalConstructor, originalConstructorR, originalName);
	QVERIFY2(filterId > 0, "filterInsert should return a valid filter id");

	//Update with a marked-invalidated flag; make sure it round-trips.
	const std::string updatedRFilter		= "filterResult <- x > 2";
	const std::string updatedGenerated		= "generated <- FALSE";
	const std::string updatedConstructor	= "{\"formulas\":[1]}";
	const std::string updatedConstructorR	= "constrR <- 2 + 2";
	const std::string updatedName			= "roundtripFilterRenamed";
	const bool		updatedInvalidated		= true;

	dbi.filterUpdate(filterId, updatedRFilter, updatedGenerated, updatedConstructor, updatedConstructorR, updatedName, updatedInvalidated);

	std::string rFilter, generatedFilter, constructorJson, constructorR, name;
	int		revision		= -1;
	bool	invalidated		= false;

	dbi.filterLoad(filterId, rFilter, generatedFilter, constructorJson, constructorR, revision, name, invalidated);

	QCOMPARE(QString::fromStdString(rFilter),			QString::fromStdString(updatedRFilter));
	QCOMPARE(QString::fromStdString(generatedFilter),	QString::fromStdString(updatedGenerated));
	QCOMPARE(QString::fromStdString(constructorJson),	QString::fromStdString(updatedConstructor));
	QCOMPARE(QString::fromStdString(constructorR),		QString::fromStdString(updatedConstructorR));
	QCOMPARE(QString::fromStdString(name),				QString::fromStdString(updatedName));
	QCOMPARE(invalidated, updatedInvalidated);
	QVERIFY2(revision >= 0, "revision must stay an integer revision, not the invalidated flag");

	dbi.filterDelete(filterId);
}

void TestAll::testCloseWorkspaceAndDataSets()
{
	QVERIFY(_newPkgWithDataSet());

	Workspace * ws = _pkg->workspace();
	QVERIFY(ws);
	QVERIFY(_pkg->dataSet());

	//Give the workspace several distinct (non-empty) datasets so deleteShownDataSet has to
	//re-pick another shown dataset after each removal. The excision, Cut 2: applySchema
	//replaces the CSVImporter fixture.
	DataSet * second = ws->createDataSet();
	QVERIFY(second);
	second->applySchema("ds-test-close-2", 3, fixtureSchema(), "");
	QVERIFY(second->columnCount() > 0);

	DataSet * third = ws->createDataSet();
	QVERIFY(third);
	third->applySchema("ds-test-close-3", 3, fixtureSchema(), "");
	QVERIFY(third->columnCount() > 0);

	QCOMPARE(ws->dataSets().size(), size_t(3));

	//Deleting the shown dataset must not crash and must leave the other datasets alive.
	DataSet * shown = ws->shownDataSet();
	QVERIFY(shown);
	ws->deleteShownDataSet();
	QCOMPARE(ws->dataSets().size(), size_t(2));
	QVERIFY(ws->shownDataSet());
	QVERIFY(ws->shownDataSet() != shown);

	//Delete the remaining ones, one at a time, until the workspace is empty. The old crash
	//(ColumnModel::shownDataSetChangedHandler disconnecting a stale dataset) used to segfault here.
	while (ws->shownDataSet())
		ws->deleteShownDataSet();

	QCOMPARE(ws->dataSets().size(), size_t(0));
	QVERIFY(!ws->shownDataSet());

	//Re-populate, then tear the whole workspace down (deleteWorkspace/reset) — must not crash either.
	DataSet * again = ws->createDataSet();
	QVERIFY(again);
	again->applySchema("ds-test-close-again", 3, fixtureSchema(), "");
	QVERIFY(ws->dataSets().size() == size_t(1));

	_pkg->deleteWorkspace();
	QVERIFY(!_pkg->workspace());

	//A fresh workspace (as DataSetPackage::createDataSet does on first use) still works afterwards.
	DataSet * fresh = _pkg->createDataSet();
	QVERIFY(fresh);
	QVERIFY(_pkg->workspace());

	//_pkg->deleteWorkspace() above destroyed the workspace `ws` pointed at; createDataSet() made a new
	//one, so re-obtain it before touching it.
	ws = _pkg->workspace();
	QVERIFY(ws);

	//Regression: after closing the workspace, opening (i.e. adding) datasets again must keep working
	//instead of targeting a stale/removed workspace. Make the fresh dataset non-empty and add a couple
	//more, then make sure the workspace holds them all and can still close them without crashing.
	fresh->applySchema("ds-test-close-fresh", 3, fixtureSchema(), "");
	QVERIFY(fresh->columnCount() > 0);

	DataSet * secondAfterClose = ws->createDataSet();
	QVERIFY(secondAfterClose);
	secondAfterClose->applySchema("ds-test-close-2b", 3, fixtureSchema(), "");
	QVERIFY(secondAfterClose->columnCount() > 0);

	DataSet * thirdAfterClose = ws->createDataSet();
	QVERIFY(thirdAfterClose);
	thirdAfterClose->applySchema("ds-test-close-3b", 3, fixtureSchema(), "");
	QVERIFY(thirdAfterClose->columnCount() > 0);

	QCOMPARE(ws->dataSets().size(), size_t(3));
	QVERIFY(ws->shownDataSet());

	while (ws->shownDataSet())
		ws->deleteShownDataSet();

	QCOMPARE(ws->dataSets().size(), size_t(0));
	QVERIFY(!ws->shownDataSet());
}

// ---------- Lane data_changed / applyRevision scenarios (data-edit-design §6) ----------
// (laneColumn/laneSchema/fixtureSchema are defined at the top of the file — the excision,
// Cut 2 moved them up when they replaced the CSVImporter fixture loading.)

DataSet * TestAll::_newLaneDataSet(const Json::Value & schema, uint64_t rows)
{
	if (_pkg)	delete _pkg;
	_pkg = nullptr;

	_pkg = new DataSetPackage(this);
	DataSet * dataSet = _pkg->createDataSet();
	dataSet->applySchema("ds-test-lane", rows, schema, "");
	return dataSet;
}

void TestAll::testLaneRevisionLandsRowsAndSchema()
{
	const Json::Value schemaA = laneSchema({
		laneColumn("score", "scale"),
		laneColumn("group", "nominal", {"A", "B"}),
	});
	DataSet * dataSet = _newLaneDataSet(schemaA, 3);

	// The open landed: lane-bound, revision space at 0, schema + rows in place.
	QVERIFY(dataSet->isOpen());
	QCOMPARE(dataSet->datasetId(),		std::string("ds-test-lane"));
	QCOMPARE(dataSet->laneRevision(),	uint64_t(0));
	QCOMPARE(dataSet->schemaRows(),		uint64_t(3));
	QCOMPARE(dataSet->schema().size(),	size_t(2));
	QVERIFY(dataSet->schemaColumn("score") != nullptr);
	QCOMPARE(int(dataSet->schemaColumn("score")->type),	int(columnType::scale));
	QCOMPARE(int(dataSet->schemaColumn("group")->type),	int(columnType::nominal));
	QCOMPARE(dataSet->schemaColumn("group")->levels.size(),	size_t(2));	// levels verbatim from the wire

	QSignalSpy restarts(dataSet, &DataSet::schemaChanged);
	QCOMPARE(restarts.count(), 0);

	// A schema-carrying push (a paste that absorbed a level): revision adopts, the new
	// levels land, and the restart signal fires — that is where GridModel restarts the
	// view lane at the new revision (the v1 whole-buffer drop).
	Json::Value schemaB = laneSchema({
		laneColumn("score", "scale"),
		laneColumn("group", "nominal", {"A", "B", "C"}),
	});
	Json::Value allInvalidation;
	allInvalidation["all"] = true;
	dataSet->applyRevision(1, 3, true, schemaB, allInvalidation);
	QCOMPARE(dataSet->laneRevision(),	uint64_t(1));
	QCOMPARE(dataSet->schemaRows(),		uint64_t(3));
	QCOMPARE(dataSet->schemaColumn("group")->levels.size(),	size_t(3));
	QCOMPARE(restarts.count(), 1);

	// A rows-only push (insert_rows: nulls move no count — §4, so the schema never ships):
	// rows land, the schema stays the last-shipped one, and the restart still fires.
	dataSet->applyRevision(2, 5, true, Json::Value(), Json::objectValue);
	QCOMPARE(dataSet->laneRevision(),	uint64_t(2));
	QCOMPARE(dataSet->schemaRows(),		uint64_t(5));
	QCOMPARE(dataSet->schema().size(),	size_t(2));
	QCOMPARE(restarts.count(), 2);
}

void TestAll::testLaneRevisionIgnoresStalePushes()
{
	DataSet * dataSet = _newLaneDataSet(laneSchema({
		laneColumn("score", "scale"),
		laneColumn("group", "nominal", {"A", "B"}),
	}), 4);
	dataSet->applyRevision(1, 4, true, Json::Value(), Json::objectValue);
	dataSet->applyRevision(2, 4, true, Json::Value(), Json::objectValue);

	QSignalSpy restarts(dataSet, &DataSet::schemaChanged);
	QCOMPARE(restarts.count(), 0);	// revisions land silently through this spy (schemaChanged fires inside applyRevision)

	// Replaying the CURRENT revision is a no-op (idempotent §6 ordering rule).
	dataSet->applyRevision(2, 999, true, Json::Value(), Json::objectValue);
	QCOMPARE(dataSet->laneRevision(),	uint64_t(2));
	QCOMPARE(dataSet->schemaRows(),		uint64_t(4));
	QCOMPARE(restarts.count(), 0);

	// An OLDER push — even one carrying a whole new schema — is superseded state: ignored.
	dataSet->applyRevision(1, 0, true, laneSchema({laneColumn("ghost", "scale")}), Json::objectValue);
	QCOMPARE(dataSet->laneRevision(),	uint64_t(2));
	QCOMPARE(dataSet->schemaRows(),		uint64_t(4));
	QVERIFY(dataSet->schemaColumn("ghost") == nullptr);
	QCOMPARE(restarts.count(), 0);

	// A dataset with no lane identity (legacy imports never see a data_changed) takes no
	// revisions at all. (Same package — a second DataSetPackage within one test would trip
	// the DatabaseInterface singleton; the workspace holds multiple datasets fine.)
	DataSet * legacy = _pkg->createDataSet();
	legacy->applyRevision(1, 7, true, laneSchema({laneColumn("x", "scale")}), Json::objectValue);
	QCOMPARE(legacy->laneRevision(),	uint64_t(0));
	QCOMPARE(legacy->schemaRows(),		uint64_t(0));
	QVERIFY(!legacy->isOpen());
}

void TestAll::testLaneRevisionSchemaSwap()
{
	DataSet * dataSet = _newLaneDataSet(laneSchema({
		laneColumn("score", "scale"),
		laneColumn("group", "nominal", {"A", "B"}),
	}), 3);

	// A schema_change lands as a data_changed carrying the FULL post-edit schema (P4: a
	// rename derives a new field name; a retype flips the wire type; level lists arrive in
	// the lane's declared order — the engine never re-sorts them).
	Json::Value swapped = laneSchema({
		laneColumn("score", "nominal", {"1.5", "2.5", "3.5"}),	// scale → nominal (a promotion)
		laneColumn("condition", "nominal", {"B", "A"}),			// renamed + levels reordered
	});
	Json::Value allInvalidation;
	allInvalidation["all"] = true;
	dataSet->applyRevision(1, 3, true, swapped, allInvalidation);

	QCOMPARE(dataSet->laneRevision(),				uint64_t(1));
	QCOMPARE(dataSet->schema().size(),				size_t(2));
	QVERIFY(dataSet->schemaColumn("group")		== nullptr);	// the old name is GONE from the schema
	QVERIFY(dataSet->schemaColumn("condition")	!= nullptr);
	QCOMPARE(int(dataSet->schemaColumn("score")->type), int(columnType::nominal));	// the retype landed
	QCOMPARE(dataSet->schemaColumn("condition")->levels.size(), size_t(2));
	QCOMPARE(dataSet->schemaColumn("condition")->levels.at(0), std::string("B"));	// wire order, verbatim
	QCOMPARE(dataSet->schemaColumn("condition")->levels.at(1), std::string("A"));
}

void TestAll::testLaneRevisionRowGrowthWithoutSchema()
{
	DataSet * dataSet = _newLaneDataSet(laneSchema({
		laneColumn("score", "scale"),
		laneColumn("group", "nominal", {"A", "B"}),
	}), 3);

	// insert_rows' return leg: rows grow, the schema is ABSENT (nulls move no count — I7),
	// and the invalidation descriptor is a plain rows_from.
	QSignalSpy restarts(dataSet, &DataSet::schemaChanged);
	Json::Value rowsFrom;
	rowsFrom["rows_from"] = 3;
	dataSet->applyRevision(1, 8, true, Json::Value(), rowsFrom);

	QCOMPARE(dataSet->laneRevision(),	uint64_t(1));
	QCOMPARE(dataSet->schemaRows(),		uint64_t(8));
	QCOMPARE(dataSet->schema().size(),	size_t(2));	// the schema never shipped — the old one stands
	QVERIFY(dataSet->schemaColumn("score") != nullptr);
	QCOMPARE(restarts.count(), 1);

	// A shrink (delete_rows) rides the same shape.
	Json::Value rowsFrom2;
	rowsFrom2["rows_from"] = 1;
	dataSet->applyRevision(2, 2, true, Json::Value(), rowsFrom2);
	QCOMPARE(dataSet->schemaRows(), uint64_t(2));
	QCOMPARE(dataSet->schema().size(), size_t(2));
}

void TestAll::testLaneRevisionOutOfOrderPushes()
{
	DataSet * dataSet = _newLaneDataSet(laneSchema({
		laneColumn("score", "scale"),
	}), 3);
	dataSet->applyRevision(1, 3, true, Json::Value(), Json::objectValue);

	QSignalSpy restarts(dataSet, &DataSet::schemaChanged);

	// A gap: revision 3 lands before 2 ever arrives (a dropped/delayed push). The
	// high-water mark adopts 3 — the dataset is at state 3, which supersedes 2 anyway.
	Json::Value schema3 = laneSchema({
		laneColumn("score", "scale"),
		laneColumn("late", "nominal", {"x"}),
	});
	dataSet->applyRevision(3, 6, true, schema3, Json::objectValue);
	QCOMPARE(dataSet->laneRevision(),	uint64_t(3));
	QCOMPARE(dataSet->schemaRows(),		uint64_t(6));
	QVERIFY(dataSet->schemaColumn("late") != nullptr);
	const int restartsAfter3 = restarts.count();
	QCOMPARE(restartsAfter3, 1);

	// The delayed revision-2 push now arrives: OLDER than the landed state — dropped,
	// no matter what it carries (mid-fill interleaves resolve the same way: the stale
	// push cannot resurrect superseded state).
	dataSet->applyRevision(2, 42, true, laneSchema({laneColumn("ghost", "scale")}), Json::Value(Json::objectValue));
	QCOMPARE(dataSet->laneRevision(),	uint64_t(3));
	QCOMPARE(dataSet->schemaRows(),		uint64_t(6));
	QVERIFY(dataSet->schemaColumn("ghost") == nullptr);
	QCOMPARE(restarts.count(), restartsAfter3);
}

void TestAll::testLaneDatasetsHaveNoMirrorColumns()
{
	// THE R2 CANARY (step 4): a lane dataset NEVER materializes legacy Columns. The mirror
	// was grow-only metadata that went stale after renames (the "edit didn't stick"
	// disease) and carried the row-sized bug class (§1e3). If this test fails, someone
	// reintroduced mirror creation on the lane path — point the consumer at schema()
	// instead.
	DataSet * dataSet = _newLaneDataSet(laneSchema({
		laneColumn("score", "scale"),
		laneColumn("group", "nominal", {"A", "B"}),
	}), 3);

	// After the open: no mirror, and the counts serve the SCHEMA (not _columns).
	QVERIFY(dataSet->columns().empty());
	QVERIFY(dataSet->column("score") == nullptr);
	QCOMPARE(dataSet->columnCount(),		2);
	QCOMPARE(int(dataSet->schema().size()),	2);

	// After a schema-carrying revision (a rename + a column DROP — the old mirror's
	// grow-only failure mode): still no mirror, counts still honest.
	dataSet->applyRevision(1, 3, true, laneSchema({
		laneColumn("points", "scale"),	// renamed (P4)
	}), Json::Value(Json::objectValue));

	QVERIFY(dataSet->columns().empty());
	QVERIFY(dataSet->column("points") == nullptr);
	QCOMPARE(dataSet->columnCount(),		1);
	QCOMPARE(dataSet->schemaColumnIndex("score"),	-1);	// the old name is GONE
	QCOMPARE(dataSet->schemaColumnIndex("points"),	0);
}

void TestAll::testLaneColumnModelServesSchema()
{
	// THE R2 CANARY 2 (the variable editor's model — GUI-side, previously UNTESTED, which
	// is exactly how the mirror removal's _virtual bug shipped: choosing a lane column
	// marked the model virtual, the editor showed an empty "new column" form, and typing
	// a name INSERTED a column instead of renaming). A schema column chosen BY NAME is a
	// real (non-virtual) choice serving the schema's metadata; an unknown name is the
	// virtual new-column state.
	_newLaneDataSet(laneSchema({
		laneColumn("score", "scale"),
		laneColumn("group", "nominal", {"A", "B"}),
	}), 3);

	ColumnModel model;

	model.setChosenColumnByName("score");
	QVERIFY(!model.isVirtual());
	QCOMPARE(model.columnNameQ(),			QString("score"));
	QCOMPARE(model.chosenColumn(),		0);
	QCOMPARE(model.currentColumnType(),	columnTypeToQString(columnType::scale));

	model.setChosenColumnByName("group");
	QVERIFY(!model.isVirtual());
	QCOMPARE(model.columnNameQ(),			QString("group"));
	QCOMPARE(model.chosenColumn(),		1);
	QCOMPARE(model.currentColumnType(),	columnTypeToQString(columnType::nominal));

	// The CLICK path (setChosenColumn(int)): resolves via the SCHEMA on lane — the legacy
	// responder (DataSetTableModel::columnName over the empty legacy model) returned "",
	// and the fallthrough opened the virtual "new column" form for every click (the
	// second shipped step-4 bug). An index at the extent stays virtual (the "+" slot).
	model.setChosenColumn(0);
	QVERIFY(!model.isVirtual());
	QCOMPARE(model.columnNameQ(),			QString("score"));
	model.setChosenColumn(2);		// one past the schema extent = the new-column slot
	QVERIFY(model.isVirtual());

	// A rename landing (a schema-carrying revision) on the CHOSEN column is served by the
	// SAME choice — the editor's fields follow the schema, no re-choose needed (the
	// refresh hook: schemaChanged → laneSchemaRefreshed → notifyColumnChanged).
	DataSet * dataSet = DataSetPackage::pkg()->dataSet();
	model.setChosenColumnByName("score");		// the column about to be renamed (P4)
	dataSet->applyRevision(1, 3, true, laneSchema({
		laneColumn("points", "scale"),			// the chosen column, renamed
		laneColumn("group", "nominal", {"A", "B"}),
	}), Json::Value(Json::objectValue));
	QCOMPARE(model.columnNameQ(), QString("points"));

	// An unknown name IS the virtual new-column state (typing then inserts).
	model.setChosenColumnByName("nope");
	QVERIFY(model.isVirtual());
}

QTEST_MAIN(TestAll)
