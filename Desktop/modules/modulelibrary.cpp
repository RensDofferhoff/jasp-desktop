#include "modulelibrary.h"

#include <QString>
#include <QDir>
#include <QFile>
#include <qjsonobject.h>

#include "appinfo.h"
#include "gui/preferencesmodel.h"
#include "dynamicmodules.h"
#include "modules/dynamicmodule.h"
#include "utilities/appdirs.h"
#include "dirs.h"
#include "utilities/dynamicruntimeinfo.h"
#include "log.h"

ModuleLibrary * ModuleLibrary::_singleton = nullptr;

ModuleLibrary::ModuleLibrary(QObject *parent)
    : QObject(parent)
{
    _singleton = this;

	if (auto *dynMods = DynamicModules::dynMods())
    {
		connect(dynMods, &DynamicModules::dynamicModuleAdded,      this, [this](Modules::DynamicModule *) {
            emitEnvironmentInfoChanged(); 
            finishInstalling();
        });
		connect(dynMods, &DynamicModules::dynamicModuleChanged,    this, [this](Modules::DynamicModule *) { emitEnvironmentInfoChanged(); });
		connect(dynMods, &DynamicModules::dynamicModuleReplaced,   this, [this](Modules::DynamicModule *, Modules::DynamicModule *) { emitEnvironmentInfoChanged(); });
    }
    // NEO gut: module install/uninstall signals came from the removed engine scheduler.
    // Module installation will be re-routed via the orchestrator (see refactor_design/GUT_TODO.md).

    if (auto *prefs = PreferencesModel::prefs())
    {
        connect(prefs, &PreferencesModel::developerModeChanged, this, [this](bool) { emitEnvironmentInfoChanged(); });
        connect(prefs, &PreferencesModel::languageCodeChanged,  this, [this]() { emitEnvironmentInfoChanged(); });
        connect(prefs, &PreferencesModel::interfaceFontChanged, this, [this]() { emitEnvironmentInfoChanged(); });
        connect(prefs, &PreferencesModel::currentThemeNameChanged,  this, [this](const QString &) { emitEnvironmentInfoChanged(); });
    }
}

QVariantMap ModuleLibrary::getEnvironmentInfo() const
{
    QVariantMap envInfo;
    envInfo["version"] = QString(AppInfo::version.asString(3).c_str());
    
    auto platform	= DynamicRuntimeInfo::getRuntimeEnvironment();
	auto arch		= DynamicRuntimeInfo::getMicroArch();
	
    std::string platformArch;
	if(platform == RuntimeEnvironment::MAC)					platformArch = arch == MicroArch::AARCH64 ? "MacOS_arm64" : "MacOS_x86_64";
	else if(platform == RuntimeEnvironment::FLATPAK)		platformArch = arch == MicroArch::AARCH64 ? "Flatpak_aarch64" : "Flatpak_x86_64";
	else if(platform == RuntimeEnvironment::LINUX_LOCAL)	platformArch = arch == MicroArch::AARCH64 ? "Flatpak_aarch64" : "Flatpak_x86_64";      // When developing within devcontainer then jaspModule files with Flatpak_x86_64 also work? No...
	else													platformArch = "Windows_x86-64";
	
    envInfo["arch"]					= tq(platformArch);
    envInfo["developerMode"]		= PreferencesModel::prefs()->developerMode();						// Preferences needed in webapp
    envInfo["theme"]				= PreferencesModel::prefs()->currentThemeName().replace("Theme", "");
    envInfo["font"]					= PreferencesModel::prefs()->interfaceFont();
    envInfo["language"]				= PreferencesModel::prefs()->languageCode().replace("_", "-");		// do replace to enforce BCP 47 language tag format
    envInfo["installedModules"]		= installedModulesInfo();
    envInfo["uninstallableModules"] = getUninstallableModules();
		
    return envInfo;
}

void ModuleLibrary::uninstallJASPModule(const QString &moduleName)
{
	if (auto *dynMods = DynamicModules::dynMods())
        dynMods->uninstallModule(moduleName.toStdString());
}

QVariantMap ModuleLibrary::installedModulesInfo() const
{
    QVariantMap installedModules;
    // NEO: the live modules (orchestrator catalog, applied by DynamicModules) — not a local
    // manifest scan.
    if (auto * dynMods = DynamicModules::dynMods())
        for (const auto & [name, module] : dynMods->modules())
            installedModules[tq(name)] = tq(module->version().asString(3));
    return installedModules;
}

QStringList ModuleLibrary::getUninstallableModules() const
{
    // Only modules installed in user modules dir are uninstallable
    auto dir = QDir(AppDirs::userModulesLibDir());
    return dir.entryList(QDir::Dirs | QDir::NoDotAndDotDot);
}

QString ModuleLibrary::getEnvironmentInfoJson() const
{
	QVariantMap envInfo = getEnvironmentInfo();
	QJsonDocument infoDoc = QJsonDocument(QJsonObject::fromVariantMap(envInfo));
		
	return infoDoc.toJson(QJsonDocument::Indented);
}

void ModuleLibrary::emitEnvironmentInfoChanged()
{
	//Log::log() << "ModuleLibrary: Environment state updated: " << getEnvironmentInfoJson().replace('\n', ' ').toStdString() << std::endl;

    emit environmentInfoChanged(getEnvironmentInfo());
}

void ModuleLibrary::startInstalling()
{
    _isInstalling = true;
    emit isInstallingChanged();
}

void ModuleLibrary::finishInstalling()
{
    _isInstalling = false;
    // NEO TODO: the old install flow cleaned up *.JASPModule bundles from the app's temp dir
    // here (cleanupTempDir). Temp/workspace state is now the orchestrator's responsibility
    // (its janitor reclaims workspaces); if a fresh-start cleanup keyed on session/work id is
    // needed, reimplement it there, not here.
    if (!_updatableModuleNames.isEmpty())
    {
        _updatableModuleNames.clear();
        emit updatableModuleNamesChanged();
    }
    emit requestModulePageRefresh();
    emit isInstallingChanged();
}

void ModuleLibrary::setUpdatableModuleNames(const QStringList &names)
{
    if (_updatableModuleNames != names)
    {
        _updatableModuleNames = names;
        emit updatableModuleNamesChanged();
    }
}