# Sourced by provision.sh: starts the Jenkins in jenkins.txt beside the
# forge it is paired with, and seeds it for tests/contract_jenkins.rs.
#
# Core Jenkins, no plugins: the war and a Temurin JRE, each checked against
# its published SHA-256. An init script (init.groovy.d) turns security on
# with one user, gfadmin, who may do anything once signed in and nothing
# before, gives them an API token, and adds two freestyle jobs:
# widgets-deploy, which takes TARGET and FAST and finishes at once, and
# widgets-slow, which sleeps until it is stopped. Build 1 of widgets-deploy
# is started and waited for, and a repository on the forge with Actions off
# gets a commit status linking to it, as Jenkins's Gitea plugin posts one.
#
# Uses from provision.sh: $here $downloads $arch $user $bind $port_offset
# $ua, and rand, api, json_field. Sets jenkins_json for the targets file.

# The build, read once. (`read` empties its variables at the end of the
# file, so each field is copied out.)
jenkins_pair=""
while read -r kind_ version_ port_ pair_ war_sha_ jre_ jre_amd64_ jre_arm64_; do
  case "$kind_" in ''|'#'*) continue ;; esac
  jenkins_pair="$pair_" jversion="$version_" jport="$port_" jwar_sha="$war_sha_" jre_release="$jre_"
  if [ "$arch" = arm64 ]; then jre_sha="$jre_arm64_"; else jre_sha="$jre_amd64_"; fi
done < "$here/jenkins.txt"

download_checked() { # url sha path: fetches url to path unless it is there
  local url="$1" sha="$2" path="$3" attempt
  if [ -f "$path" ] && echo "$sha  $path" | sha256sum --check --quiet >/dev/null 2>&1; then
    return 0
  fi
  for attempt in 1 2 3; do
    curl --fail --silent --show-error --location -A "$ua" --retry 5 --retry-all-errors \
      -o "$path.part" "$url" >&2 || true
    if echo "$sha  $path.part" | sha256sum --check --quiet >&2 2>/dev/null; then
      mv "$path.part" "$path"
      return 0
    fi
    echo "download $attempt of $url failed or did not match its checksum" >&2
    rm -f "$path.part"
    sleep 5
  done
  return 1
}

fetch_jenkins() { # sets jenkins_war and jenkins_java
  jenkins_war="$downloads/jenkins-$jversion.war"
  download_checked "https://get.jenkins.io/war-stable/$jversion/jenkins.war" "$jwar_sha" "$jenkins_war"
  local jarch=x64 v file_v jre_dir archive
  [ "$arch" = arm64 ] && jarch=aarch64
  v="${jre_release#jdk-}"
  file_v="${v/+/_}"
  jre_dir="$downloads/jre-$v-$arch"
  if [ ! -x "$jre_dir/bin/java" ]; then
    archive="$jre_dir.tar.gz"
    download_checked \
      "https://github.com/adoptium/temurin21-binaries/releases/download/${jre_release/+/%2B}/OpenJDK21U-jre_${jarch}_linux_hotspot_${file_v}.tar.gz" \
      "$jre_sha" "$archive"
    rm -rf "$jre_dir"
    mkdir -p "$jre_dir"
    tar -xzf "$archive" -C "$jre_dir" --strip-components=1
    rm -f "$archive"
  fi
  jenkins_java="$jre_dir/bin/java"
}

jenkins_init_script() {
  cat <<'EOF'
// Runs once, at the first start: security, the user and their API token,
// and the jobs. Everything here is in core.
import hudson.model.*
import hudson.security.*
import hudson.tasks.Shell
import jenkins.model.*
import jenkins.security.ApiTokenProperty

def jenkins = Jenkins.get()
def realm = new HudsonPrivateSecurityRealm(false)
def user = realm.createAccount("gfadmin", System.getenv("JENKINS_CONTRACT_PASSWORD"))
jenkins.setSecurityRealm(realm)
def strategy = new FullControlOnceLoggedInAuthorizationStrategy()
strategy.setAllowAnonymousRead(false)
jenkins.setAuthorizationStrategy(strategy)
jenkins.setNumExecutors(2)
JenkinsLocationConfiguration.get().setUrl(System.getenv("JENKINS_CONTRACT_URL"))

def deploy = jenkins.createProject(FreeStyleProject, "widgets-deploy")
deploy.addProperty(new ParametersDefinitionProperty(
    new StringParameterDefinition("TARGET", "dev", "where to deploy"),
    new BooleanParameterDefinition("FAST", false, "skip the checks")))
deploy.getBuildersList().add(new Shell('echo "contract-jenkins-deploy to $TARGET"'))
deploy.save()
def slow = jenkins.createProject(FreeStyleProject, "widgets-slow")
slow.getBuildersList().add(new Shell("sleep 600"))
slow.save()
jenkins.save()

def tokens = user.getProperty(ApiTokenProperty)
if (tokens == null) {
    tokens = new ApiTokenProperty()
    user.addProperty(tokens)
}
def token = tokens.getTokenStore().generateNewToken("contract")
user.save()
new File(System.getenv("JENKINS_CONTRACT_TOKEN_FILE")).text = token.plainValue
EOF
}

jk_api() { # method url credential [form]
  if [ -n "${4:-}" ]; then
    curl --fail-with-body --silent --show-error -X "$1" -A "$ua" -u "$3" --data "$4" "$2"
  else
    curl --fail-with-body --silent --show-error -X "$1" -A "$ua" -u "$3" "$2"
  fi
}

seed_jenkins() { # forge-dir forge-base admin-token
  local fdir="$1" fbase="$2" ftoken="$3"
  local port=$((jport + port_offset))
  local jdir="$fdir/jenkins" jbase="http://127.0.0.1:$port"
  echo "== jenkins-$jversion on port $port, beside $jenkins_pair" >&2
  fetch_jenkins
  mkdir -p "$jdir/home/init.groovy.d"
  jenkins_init_script > "$jdir/home/init.groovy.d/contract.groovy"
  chown -R "$user" "$jdir"

  local password token
  password="$(rand)"
  runuser -u "$user" -- env HOME="/home/$user" JENKINS_HOME="$jdir/home" \
    JENKINS_CONTRACT_PASSWORD="$password" JENKINS_CONTRACT_TOKEN_FILE="$jdir/token" \
    JENKINS_CONTRACT_URL="$jbase/" \
    nohup "$jenkins_java" -Xmx512m -Djava.awt.headless=true \
    -Djenkins.install.runSetupWizard=false -Dhudson.model.UsageStatistics.disabled=true \
    -jar "$jenkins_war" --httpPort="$port" --httpListenAddress="$bind" \
    >"$jdir/jenkins.log" 2>&1 &
  for _ in $(seq 1 180); do
    [ -s "$jdir/token" ] && curl --silent --fail -A "$ua" -u "gfadmin:$password" \
      "$jbase/api/json?tree=mode" >/dev/null 2>&1 && break
    sleep 1
  done
  [ -s "$jdir/token" ] || {
    echo "jenkins did not start; its log:" >&2
    tail -50 "$jdir/jenkins.log" >&2 || true
    exit 1
  }
  token="$(cat "$jdir/token")"

  # Build 1 of widgets-deploy, with a parameter, finished before the tests.
  jk_api POST "$jbase/job/widgets-deploy/buildWithParameters?delay=0sec" "gfadmin:$token" \
    "TARGET=staging" >/dev/null
  local result=""
  for _ in $(seq 1 60); do
    result="$(jk_api GET "$jbase/job/widgets-deploy/1/api/json?tree=result" "gfadmin:$token" 2>/dev/null \
      | json_field result 2>/dev/null || true)"
    case "$result" in SUCCESS|FAILURE|ABORTED) break ;; esac
    sleep 1
  done
  [ "$result" = SUCCESS ] || {
    echo "jenkins build 1 of widgets-deploy ended '$result', expected SUCCESS; the log:" >&2
    tail -30 "$jdir/jenkins.log" >&2 || true
    exit 1
  }

  # The repository whose CI is Jenkins's alone, and the status the Gitea
  # plugin would post for that build.
  local sha
  api POST "$fbase/api/v1/user/repos" "$ftoken" \
    '{"name":"jenkins-widgets","private":true,"auto_init":true,"default_branch":"main"}' >/dev/null
  api PATCH "$fbase/api/v1/repos/gfadmin/jenkins-widgets" "$ftoken" '{"has_actions":false}' >/dev/null
  sha="$(api GET "$fbase/api/v1/repos/gfadmin/jenkins-widgets/branches/main" "$ftoken" | json_field commit.id)"
  api POST "$fbase/api/v1/repos/gfadmin/jenkins-widgets/statuses/$sha" "$ftoken" \
    "{\"state\":\"success\",\"context\":\"gfadmin/jenkins-widgets/pipeline/head\",\"target_url\":\"$jbase/job/widgets-deploy/1/display/redirect\",\"description\":\"This commit looks good\"}" >/dev/null

  jenkins_json="$(printf '{"version":"%s","base_url":"%s","user":"gfadmin","token":"%s","password":"%s","repo":"jenkins-widgets","sha":"%s"}' \
    "$jversion" "$jbase" "$token" "$password" "$sha")"
}
