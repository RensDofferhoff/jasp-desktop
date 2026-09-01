# This will build the CommonData library
#
# On Linux,
#   - We need to link to the `librt.so`, and for some reason, CMake cannot find it
#     easily, so, Libraries module tries to find it and provides it to CommonData here.
#   - Since we are not using Conan, I link to the libjsoncpp, using the PkgConfig variables
#
#
list(APPEND CMAKE_MESSAGE_CONTEXT CommonData)

# The excision, Cut 3: internalDbDefinition.sql / createIndexes.sql (and their
# make_includable headers) died with DatabaseInterface.

make_includable(../Engine/jaspBase/R/distributionSamplers.R				jaspBase_distributionSamplers.h)
make_includable(../Engine/jaspBase/R/friendlyConstructorFunctions.R		jaspBase_friendlyConstructorFunctions.h)
make_includable(../Engine/jaspBase/R/transformFunctions.R				jaspBase_transformFunctions.h)
make_includable(../Engine/jaspBase/R/transformBoxCox.R					jaspBase_transformBoxCox.h)
make_includable(../Engine/jaspBase/R/transformJohnson.R					jaspBase_transformJohnson.h)
make_includable(../Engine/jaspBase/R/transformYeoJohnson.R				jaspBase_transformYeoJohnson.h)
make_includable(../Engine/jaspBase/R/transformPower.R					jaspBase_transformPower.h)


file(GLOB_RECURSE HEADER_FILES	"${CMAKE_CURRENT_LIST_DIR}/*.h")
file(GLOB_RECURSE SOURCE_FILES	"${CMAKE_CURRENT_LIST_DIR}/*.cpp")

file(GLOB JASPRESULTS_HEADER_FILES "${CMAKE_CURRENT_LIST_DIR}/jaspBase/src/*.h")
file(GLOB JASPRESULTS_SOURCE_FILES "${CMAKE_CURRENT_LIST_DIR}/jaspBase/src/*.cpp")

# this target is not compiled and only exists so that the sources and headers of jaspResults show up in the project and can be opened with Ctrl+k
add_custom_target(jaspResults ALL SOURCES ${JASPRESULTS_SOURCE_FILES} ${JASPRESULTS_HEADER_FILES})


add_library(CommonData STATIC ${SOURCE_FILES} ${HEADER_FILES})

#add_dependencies(CommonData R-Interface)

# NEO gut: R-Interface includes/links and boost::interprocess definitions removed.
# CommonData no longer embeds R or talks shared-memory IPC.
target_include_directories(
	CommonData
	PUBLIC # JASP
	${PROJECT_SOURCE_DIR}/Common
	# R
	${R_INCLUDE_PATH}
	${R_HOME_PATH}/include
	${RCPP_PATH}/include
	# The excision aftermath (2026-09-02): ${Boost_INCLUDE_DIRS} died — nothing includes boost.
	)

target_link_libraries(
    CommonData
    PUBLIC
	Qt::Core
	Qt::Gui
    Qt::Qml
    Qt::Sql
    Qt::Quick
    # The excision, Cut 3: LibArchive (ArchiveReader — .jasp zip reading) and SQLite
    # (DatabaseInterface) died together; .jasp persistence returns in a later NEO era.
)
if(MSVC)
  target_compile_options(CommonData PRIVATE /EHsc)
endif()
target_compile_definitions(
  CommonData
  PUBLIC 
  $<$<BOOL:${JASP_PRINT_ENGINE_MESSAGES}>:PRINT_ENGINE_MESSAGES>
  BUILDING_JASP_ENGINE R_HOME=${R_HOME_PATH}
  $<$<BOOL:${JASP_TIMER_USED}>:PROFILE_JASP>
  JSONCPP_NO_LOCALE_SUPPORT	
  JASP_USES_QT_HERE
)

if(WINDOWS)
  target_compile_definitions(
    CommonData
    PUBLIC BOOST_WINDOWS
           NOMINMAX
           WIN32_LEAN_AND_MEAN)
endif()

if(IWYU_EXECUTABLE AND RUN_IWYU)
  set_target_properties(CommonData PROPERTIES CXX_INCLUDE_WHAT_YOU_USE
                                          ${IWYU_EXECUTABLE})
endif()

list(POP_BACK CMAKE_MESSAGE_CONTEXT)
