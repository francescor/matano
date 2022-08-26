import * as path from "path";
import * as fs from "fs";
import { Command, Flags } from "@oclif/core";
import { prompt } from "enquirer";
import execa from "execa";
import ora from "ora";
import * as YAML from "yaml";

import * as AWS from "aws-sdk";
import { VpcNetworkContextProviderPlugin } from "aws-cdk/lib/context-providers/vpcs";
import { SdkProvider } from "aws-cdk/lib/api/aws-auth/sdk-provider";
import { readConfig } from "../util";
import BaseCommand from "../base";



export default class RefreshContext extends BaseCommand {
  static description = "Refreshes Matano context.";

  static examples = [
    `matano refresh-context`,
    `matano refresh-context --profile prod`,
    `matano refresh-context --profile prod --user-directory my-matano-directory`,
    `matano refresh-context --profile prod --region eu-central-1 --account 12345678901`,
  ];

  static flags = {
    profile: Flags.string({
      char: "p",
      description: "AWS Profile to use for credentials.",
    }),
    account: Flags.string({
      char: "a",
      description: "AWS Account to deploy to.",
    }),
    region: Flags.string({
      char: "r",
      description: "AWS Region to deploy to.",
    }),
    "user-directory": Flags.string({
      required: false,
      description: "Matano user directory to use.",
    }),
  };

  static async refreshMatanoContext(userDirectory: string, awsAccount: string, awsRegion: string, awsProfile?: string) {
    const contextFilepath = path.join(userDirectory, "matano.context.json");

    const config = readConfig(userDirectory, "matano.config.yml");
    const configVpcId: string | undefined = config?.vpc?.id;

    // Needed to make SSO creds loading work.
    process.env.AWS_SDK_LOAD_CONFIG = "1";

    const sdkProvider = await SdkProvider.withAwsCliCompatibleDefaults({ profile: awsProfile, });

    const vpcNetworkContextProviderPlugin = new VpcNetworkContextProviderPlugin(sdkProvider);

    const vpcFilters: any = configVpcId ? { "vpc-id": configVpcId } : { "is-default": "true" };

    const vpcContext = await vpcNetworkContextProviderPlugin.getValue({
      account: awsAccount,
      region: awsRegion,
      filter: vpcFilters,
    });

    const output = { "//": "This file is autogenerated by matano. Run `matano refresh-context` to update.", vpc: vpcContext, };
    fs.writeFileSync(contextFilepath, JSON.stringify(output, null, 2));
    return output;
  }

  async run(): Promise<void> {
    const { args, flags } = await this.parse(RefreshContext);
    const { profile } = flags;
    const userDirectory = this.validateGetMatanoDir(flags);
    const {awsAccountId, awsRegion} = this.validateGetAwsRegionAccount(flags, userDirectory);

    this.debug(userDirectory);

    const spinner = ora("Refreshing context...").start();

    await RefreshContext.refreshMatanoContext(userDirectory, awsAccountId, awsRegion, profile);

    spinner.succeed("Successfully refreshed.");
  }
}
