# Conan.cmake tries to run the `conan install` command using the right
# parameters. If everything goes right, you don't need to do anything,
# and CMake and Conan should handle all the dependencies properly. However,
# if you have any issues with Conan, you need to get your hand dirty, and
# acutally run the `conan install` command.
#
# In general, it's better if users are running their command, for now,
# if this works, I would like to handle it more automatically, but this
# turns out to be complicated or problematic, I will remove this and
# add a step to the build guide.
#
# As for what happens here, Conan download and build the necessary libraries,
# and if everything goes right, it generates several Find*.cmake files in the
# build folder, and these will be used by CMake and Libraries.cmake to find
# and link necessary libraries to JASP.

list(APPEND CMAKE_MESSAGE_CONTEXT Conan)

if(USE_CONAN)

  message(CHECK_START "Configuring Conan")
  set(CONAN_FILE_PATH ${CMAKE_SOURCE_DIR})

  message(STATUS "  ${CMAKE_BUILD_TYPE}")
  set(CONAN_COMPILER_RUNTIME "dynamic")

  # When using RelWithDebInfo or MinSizeRel, generate a Conan profile that
  # sets the consumer build type while forcing all dependencies to Release.
  # This avoids slow/broken dependency builds and reuses cached Release binaries.
  if(CMAKE_BUILD_TYPE STREQUAL "RelWithDebInfo" OR CMAKE_BUILD_TYPE STREQUAL "MinSizeRel" OR CMAKE_BUILD_TYPE STREQUAL "Debug")
    set(CONAN_PROFILE_PATH "${CMAKE_BINARY_DIR}/_conan_build/relwithdebinfo_override.profile")
    file(WRITE "${CONAN_PROFILE_PATH}"
"include(default)

[settings]
build_type=${CMAKE_BUILD_TYPE}
*:build_type=Release
&:build_type=${CMAKE_BUILD_TYPE}
")
    # For conan install: use profile so JASP builds as RelWithDebInfo, deps as Release
    set(CONAN_INSTALL_BUILD_TYPE_ARGS "--profile=${CONAN_PROFILE_PATH}")
  else()
    set(CONAN_INSTALL_BUILD_TYPE_ARGS "-s build_type=${CMAKE_BUILD_TYPE}")
  endif()

  # Parse the args strings into CMake lists so each flag becomes a separate
  # argument in execute_process(COMMAND ...). Without this, "-s build_type=X"
  # is passed as a single combined argument and Conan rejects it.
  separate_arguments(CONAN_INSTALL_BUILD_TYPE_ARGS NATIVE_COMMAND "${CONAN_INSTALL_BUILD_TYPE_ARGS}")

  if(JASP_SYNTAX_INTERFACE_ONLY)
    set(CONAN_SYNTAX_OPTION "-o syntax_interface_only=True")
  else()
    set(CONAN_SYNTAX_OPTION "")
  endif()

  # We use our own recipe with some patches to cook up a functional version of freexl, so get the recipe:
  # The excision, Cut 2: the freexl recipe provisioning died with the importers (nothing
  # links freexl anymore — see conanfile.py).

  # Configure Conan for windows
  if(WIN32)
    set(CONAN_RESULT_FILE "conanbuild.bat") #for windows

    message(STATUS "  ${CONAN_COMPILER_RUNTIME}")

    # The excision, Cut 2: the freexl conan-create step died with the importers.

    # Clean stale Conan-generated CMake files so CMakeDeps creates fresh find
    # modules with correct paths for the resolved build types.
    file(GLOB _CONAN_CMAKE_FILES "${CMAKE_BINARY_DIR}/_conan_build/*.cmake" "${CMAKE_BINARY_DIR}/_conan_build/conanbuild*")
    if(_CONAN_CMAKE_FILES)
      file(REMOVE ${_CONAN_CMAKE_FILES})
    endif()

    execute_process(
      COMMAND_ECHO STDOUT
      WORKING_DIRECTORY ${CMAKE_BINARY_DIR}
      COMMAND
      conan install ${CONAN_FILE_PATH} --output-folder=${CMAKE_BINARY_DIR}/_conan_build
      ${CONAN_INSTALL_BUILD_TYPE_ARGS}
      -c tools.cmake.cmaketoolchain:generator=${CMAKE_GENERATOR}
      -s compiler.runtime=${CONAN_COMPILER_RUNTIME} --build=missing
      ${CONAN_SYNTAX_OPTION})

    set_property(DIRECTORY APPEND PROPERTY ADDITIONAL_CLEAN_FILES _deps)

      # configure conan for apple
  elseif(APPLE)

    set(CONAN_RESULT_FILE "conanbuild.sh")

    # We set CC and CCX to nothing because that was the only difference between running conan in a terminal (where it worked) and in qt creator (where it did not) 
    # They were set to bona fide looking xtools stuff but apparently this was too much for conan.

    # The excision, Cut 2: the freexl conan-create step died with the importers.

    execute_process(
        COMMAND_ECHO STDOUT
        WORKING_DIRECTORY ${CMAKE_BINARY_DIR}
        COMMAND zsh -c -l "export CC=\"\"; export CCX=\"\"; conan install ${CONAN_FILE_PATH} -s build_type=${CMAKE_BUILD_TYPE} -s os.version=${CMAKE_OSX_DEPLOYMENT_TARGET} --build=missing ${CONAN_SYNTAX_OPTION} -of ${CMAKE_BINARY_DIR}/_conan_build")

  endif()

  if(EXISTS ${CMAKE_BINARY_DIR}/_conan_build/${CONAN_RESULT_FILE})
    message(CHECK_PASS "successful")
  else()
    message(CHECK_FAIL "unsuccessful")
    message(
      FATAL_ERROR
        "Conan configuration failed. You may try running the above conan command from your command line, in your build directory."
    )
  endif()

  include(${CMAKE_BINARY_DIR}/_conan_build/conan_toolchain.cmake)

  set_property(DIRECTORY APPEND PROPERTY ADDITIONAL_CLEAN_FILES _deps)
endif()

list(POP_BACK CMAKE_MESSAGE_CONTEXT)
