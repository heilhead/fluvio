use clap::Parser;
use crate::Result;
use fluvio::Fluvio;
use fluvio::metadata::derivedstream::DerivedStreamSpec;

#[derive(Debug, Parser)]
pub struct DeleteDerivedStreamOpt {
    name: String,
}

impl DeleteDerivedStreamOpt {
    pub async fn process(self, fluvio: &Fluvio) -> Result<()> {
        let admin = fluvio.admin().await;
        admin.delete::<DerivedStreamSpec, _>(&self.name).await?;
        Ok(())
    }
}
