#!/bin/sh
# Deploy the service to Cloud Run. Run by hand after reviewing the cost:
# Cloud Run scales to zero; Firestore and Cloud Storage have free tiers.
#
#   PROJECT=crate-digger-tr GOOGLE_CLIENT_IDS=<desktop OAuth client id> scripts/deploy.sh
#
# Needs: gcloud signed in with owner rights on PROJECT, billing linked.
set -eu
: "${PROJECT:?set PROJECT}"
: "${GOOGLE_CLIENT_IDS:?set GOOGLE_CLIENT_IDS}"
REGION=${REGION:-europe-west2}
BUCKET=${BUCKET:-$PROJECT-backups}
SA=cd-service@$PROJECT.iam.gserviceaccount.com

gcloud config set project "$PROJECT"
gcloud services enable run.googleapis.com firestore.googleapis.com storage.googleapis.com \
  artifactregistry.googleapis.com cloudbuild.googleapis.com iam.googleapis.com

# Firestore in native mode, once.
gcloud firestore databases describe --database='(default)' >/dev/null 2>&1 \
  || gcloud firestore databases create --location="$REGION" --type=firestore-native

# Private bucket for backups: uniform access, no public access.
gcloud storage buckets describe "gs://$BUCKET" >/dev/null 2>&1 \
  || gcloud storage buckets create "gs://$BUCKET" --location="$REGION" \
       --uniform-bucket-level-access --public-access-prevention

# The service's own identity, with only what it needs.
gcloud iam service-accounts describe "$SA" >/dev/null 2>&1 \
  || gcloud iam service-accounts create cd-service --display-name="Crate Digger service"
gcloud projects add-iam-policy-binding "$PROJECT" --member="serviceAccount:$SA" --role=roles/datastore.user --condition=None >/dev/null
gcloud storage buckets add-iam-policy-binding "gs://$BUCKET" --member="serviceAccount:$SA" --role=roles/storage.objectAdmin >/dev/null

gcloud run deploy cd-service --source . --region "$REGION" --service-account "$SA" \
  --allow-unauthenticated --min-instances 0 --max-instances 3 --memory 512Mi \
  --set-env-vars "GCP_PROJECT=$PROJECT,BACKUP_BUCKET=$BUCKET,GOOGLE_CLIENT_IDS=$GOOGLE_CLIENT_IDS"

echo "Deployed. Every route except /healthz still requires a Google ID token."
echo "Set a budget alert: gcloud billing budgets create --billing-account=<id> --display-name='Crate Digger' --budget-amount=10GBP"
