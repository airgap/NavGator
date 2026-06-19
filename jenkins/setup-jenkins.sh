#!/usr/bin/env bash
# Stand up navgator's Jenkins job(s) — job-as-code, mirroring lyku/jenkins/setup-jenkins.sh.
#
# Env: JENKINS_URL (default http://localhost:8080), JENKINS_USER (default muzzin),
#      JENKINS_TOKEN (a Jenkins API token — required).
# Needs: java + curl. The job builds https://github.com/airgap/NavGator.git (branch dev)
# via the root Jenkinsfile (tri-platform matrix; runner labels: linux, macos, windows).
set -euo pipefail

JENKINS_URL="${JENKINS_URL:-http://localhost:8080}"
JENKINS_USER="${JENKINS_USER:-muzzin}"
: "${JENKINS_TOKEN:?set JENKINS_TOKEN (a Jenkins API token)}"

CLI="/tmp/jenkins-cli.jar"
[ -f "$CLI" ] || curl -fsSL "$JENKINS_URL/jnlpJars/jenkins-cli.jar" -o "$CLI"
DIR="$(cd "$(dirname "$0")" && pwd)"
auth=(-s "$JENKINS_URL" -auth "$JENKINS_USER:$JENKINS_TOKEN")

java -jar "$CLI" "${auth[@]}" who-am-i >/dev/null

for job in navgator-ci; do
    echo "  creating/updating $job ..."
    java -jar "$CLI" "${auth[@]}" create-job "$job" < "$DIR/job-configs/$job.xml" 2>/dev/null \
        || java -jar "$CLI" "${auth[@]}" update-job "$job" < "$DIR/job-configs/$job.xml"
done

echo "Done. Trigger a build with:"
echo "  java -jar $CLI -s $JENKINS_URL -auth $JENKINS_USER:\$JENKINS_TOKEN build navgator-ci"
