# T-556 spike: environment for running the wrapped gr-lora_sdr (source this).
# GR_PREFIX is where build.sh installed the OOT; GR_PY is GNU Radio's own interpreter
# (Homebrew bottles gnuradio in its own venv - a system python3 cannot import it).
export T556_WORK=${T556_WORK:-$HOME/.hackriff-ops/work/T-556}
export GR_PY=${GR_PY:-/opt/homebrew/opt/gnuradio/libexec/venv/bin/python}
export PYTHONPATH=$T556_WORK/prefix/lib/python3.14/site-packages:$T556_WORK/pydeps${PYTHONPATH:+:$PYTHONPATH}
