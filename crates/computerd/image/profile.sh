export NPM_CONFIG_PREFIX="$HOME/.local"
export PIP_USER=1
export PIP_BREAK_SYSTEM_PACKAGES=1
case ":$PATH:" in
  ":$HOME/.local/bin:"*) ;;
  *) PATH="$HOME/.local/bin:$PATH" ;;
esac
export PATH
