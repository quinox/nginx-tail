#!/bin/bash

banner() {

	if command -v figlet >/dev/null
	then
		tput setaf 3
		tput bold
		figlet "$*"
		echo ""
	else
		text="$*"
		length=$(( ${#text} + 4 ))
		tput setaf 4
		printf '#%.0s' $(seq 1 $length)
		printf "\n"

		printf "# %s%s%s #\n" "$(tput setaf 3)" "$text" "$(tput setaf 4)"

		printf '#%.0s' $(seq 1 $length)
		printf "\n"
	fi
	tput sgr0
}

die() {

	>&2 echo $'\n'"$(tput setaf 1)$(tput bold)Fatal error:$(tput dim) $1$(tput sgr0)"
	shift
	[[ $# -gt 0 ]] && {
		echo ""
		echo "$*"
	}
	exit 9
}
