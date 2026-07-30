#!/usr/bin/env python3
"""
Comprehensive tester for the "performing analysis + SPSS" itembank subset.
Tests each item by loading data into JASP, running the appropriate analysis,
and comparing the result against the expected answer.
"""

import csv
import urllib.request
import urllib.parse
import urllib.error
import re
import json
import sys
import os
import traceback
import time

# --- Configuration ---
CSV_PATH = '/home/sp42/jasp-desktop/performing_analysis_spss.csv'
DATA_DIR = '/home/sp42/jasp-desktop/test_data'
RESULTS_LOG = '/home/sp42/jasp-desktop/test_results.jsonl'

os.makedirs(DATA_DIR, exist_ok=True)

# --- RPC helpers ---
def jasp_rpc(method, params=None):
    """Call a JASP RPC method."""
    url = "http://localhost:8080/rpc"
    payload = {
        "jsonrpc": "2.0",
        "method": method,
        "params": params or {},
        "id": 1
    }
    data = json.dumps(payload).encode('utf-8')
    req = urllib.request.Request(url, data=data, headers={'Content-Type': 'application/json'})
    try:
        resp = urllib.request.urlopen(req, timeout=120)
        return json.loads(resp.read().decode('utf-8'))
    except Exception as e:
        return {"error": str(e)}

def jasp_create(module, analysis):
    """Create analysis and return its ID."""
    r = jasp_rpc("jasp_analysis_create", {"module": module, "analysis": analysis})
    if "error" in r and r["error"]:
        print(f"  ERROR creating analysis: {r['error']}")
        return None
    # Result may be nested
    result = r.get("result", r)
    if isinstance(result, dict):
        return result.get("analysisId") or result.get("id")
    return result

def jasp_run(analysis_id, options, timeout_ms=60000):
    """Set options and run analysis."""
    r = jasp_rpc("jasp_analysis_run", {
        "analysisId": analysis_id,
        "options": options,
        "wait": True,
        "timeoutMs": timeout_ms
    })
    if "error" in r and r["error"]:
        return r
    result = r.get("result", r)
    # Check if still running
    if isinstance(result, dict) and result.get("status") == "running":
        print(f"  Still running, polling...")
        r2 = jasp_rpc("jasp_analysis_results", {
            "analysisId": analysis_id,
            "wait": True,
            "timeoutMs": 60000
        })
        result = r2.get("result", r2)
    return result

def jasp_load_data(csv_path, timeout_ms=30000):
    """Load a CSV file into JASP."""
    r = jasp_rpc("jasp_data_load", {
        "path": csv_path,
        "wait": True,
        "timeoutMs": timeout_ms,
        "delimiter": ","
    })
    return r.get("result", r)

def deep_get(d, path, default=None):
    """Get a nested value from a dict using dot-notation path."""
    if d is None:
        return default
    if isinstance(path, str):
        path = path.split(".")
    for key in path:
        if isinstance(d, dict):
            d = d.get(key, default)
        elif isinstance(d, list):
            try:
                idx = int(key)
                d = d[idx] if idx < len(d) else default
            except (ValueError, IndexError):
                return default
        else:
            return default
    return d

# Store created analyses for later lookup
analysis_cache = {}

def read_jasp_result(results, path_templates):
    """Try multiple paths to extract a result value."""
    for path in path_templates:
        val = deep_get(results, path)
        if val is not None:
            return val
    return None

# --- Section routing ---
def classify_item(section_str, question_text=""):
    """
    Determine JASP module, analysis, and key options from the section string.
    Returns dict with module, analysis, options, and result_key.
    """
    sections = [s.strip() for s in section_str.split(",")]
    
    result = {
        "module": None,
        "analysis": None,
        "options": {},
        "result_paths": [],
        "skip_jasp": False,
        "skip_reason": "",
        "note": ""
    }
    
    top = sections[0].split("/")[0] if sections else ""
    
    # --- Factor analysis ---
    if top == "Factor analysis":
        result["module"] = "jaspFactor"
        
        # Determine if we need EFA or PCA
        # For "Explained variance" section with eigenvalues -> PCA
        # For "Exploratory factor analysis" -> EFA
        needs_pca = False
        for s in sections:
            if "Eigenvalues" in s or "Explained variance" in s:
                needs_pca = True
        
        if needs_pca:
            result["analysis"] = "PrincipalComponentAnalysis"
            result["options"] = {
                "componentCountMethod": "eigenvalues",
                "eigenvaluesAbove": 0,
                "rotationMethod": "orthogonal",
                "orthogonalSelector": "none"
            }
        else:
            result["analysis"] = "ExploratoryFactorAnalysis"
            result["options"] = {
                "factoringMethod": "principalAxis",
                "rotationMethod": "orthogonal",
                "orthogonalSelector": "varimax"
            }
        
        # Determine what result to extract
        if "Explained variance" in section_str:
            result["result_paths"] = [
                "results.eigenValuesContainer.collection.eigenValuesContainer_eigenValuesTable.data.2.propU"
            ]
        elif "Eigenvalues" in section_str:
            result["result_paths"] = [
                "results.eigenValuesContainer.collection.eigenValuesContainer_eigenValuesTable.data.2.eigenvalue"
            ]
        elif "Structure matrix" in section_str:
            result["result_paths"] = [
                "results.loadingsContainer.collection.loadingsContainer_structureMatrix.data"
            ]
        elif "Pattern matrix" in section_str:
            result["result_paths"] = [
                "results.loadingsContainer.collection.loadingsContainer_patternMatrix.data"
            ]
        elif "Factor correlation matrix" in section_str:
            result["result_paths"] = [
                "results.loadingsContainer.collection.loadingsContainer_factorCorrelations.data"
            ]
        elif "Factor loadings" in section_str:
            result["result_paths"] = [
                "results.loadingsContainer.collection.loadingsContainer_patternMatrix.data"
            ]
        
        return result
    
    # --- Reliability ---
    if top == "Reliability":
        result["module"] = "jaspReliability"
        result["analysis"] = "UnidimensionalReliabilityFrequentist"
        
        if "Split half" in section_str:
            result["options"]["splitHalf"] = True
        
        if "Cronbach's alpha" in section_str:
            result["result_paths"] = [
                "results.alphaContainer.collection.alphaContainer_alphaTable.data.0.Cronbach's Alpha"
            ]
        elif "Split half" in section_str:
            result["result_paths"] = [
                "results.splitHalfContainer.collection.splitHalfContainer_splitHalfTable.data"
            ]
        elif "Descriptives" in section_str:
            result["result_paths"] = [
                "results.descriptivesContainer.collection.descriptivesContainer_itemTable.data"
            ]
        
        return result
    
    # --- Assumptions ---
    if top == "Assumptions":
        if "Sphericity" in section_str:
            result["module"] = "jaspAnova"
            result["analysis"] = "AnovaRepeatedMe
